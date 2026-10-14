// SPDX-License-Identifier: Apache-2.0

//! `moreland stop`, `moreland start` and `moreland status`, for a daemon that
//! is already running (typically under systemd).
//!
//! The daemon listens on a Unix socket in `$XDG_RUNTIME_DIR`. `stop` ends the
//! session and marks the tablet stopped; the daemon keeps running and keeps
//! watching. A stopped tablet is not streamed to again until it is physically
//! reconnected — it drops out of adb's device list and comes back — or until
//! `moreland start`. Stopping is in memory only: a restarted daemon starts
//! fresh.

use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::tracker::Device;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Stop,
    Start,
    Status,
}

impl Request {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "stop" => Some(Self::Stop),
            "start" => Some(Self::Start),
            "status" => Some(Self::Status),
            _ => None,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Start => "start",
            Self::Status => "status",
        }
    }
}

/// `$XDG_RUNTIME_DIR/moreland.sock`, which only this user can reach. Without
/// a runtime directory, a per-user name in `/tmp`.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("moreland.sock"),
        // SAFETY: getuid cannot fail.
        _ => PathBuf::from(format!("/tmp/moreland-{}.sock", unsafe { libc::getuid() })),
    }
}

#[derive(Default)]
struct State {
    /// Stopped by command; skipped until they leave the device list.
    stopped: BTreeSet<String>,
    /// Ready devices in the latest list, so `stop` covers a tablet between
    /// sessions (backing off after a failure) as well as one streaming.
    ready: Vec<String>,
    /// The device a session is running for.
    active: Option<String>,
}

/// The daemon's side: what the main loop consults, and what the socket
/// thread changes.
pub struct Control {
    state: Mutex<State>,
    /// Handed to `session::run` as its stop flag. Set by `stop` (and by
    /// SIGINT/SIGTERM, which register on it too).
    session_stop: Arc<AtomicBool>,
}

impl Control {
    pub fn new(session_stop: Arc<AtomicBool>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            session_stop,
        }
    }

    /// Take in a fresh device list. A stopped device that is no longer in it
    /// was unplugged, which lifts the stop.
    pub fn observe(&self, devices: &[Device]) {
        let mut state = self.state.lock().unwrap();
        state.ready = devices
            .iter()
            .filter(|d| d.is_ready())
            .map(|d| d.serial.clone())
            .collect();
        state.stopped.retain(|serial| {
            let present = devices.iter().any(|d| &d.serial == serial);
            if !present {
                tracing::info!("{serial} was disconnected; it will stream again when reconnected");
            }
            present
        });
    }

    pub fn is_stopped(&self, serial: &str) -> bool {
        self.state.lock().unwrap().stopped.contains(serial)
    }

    /// Mark a session as starting. False if the device was stopped in the
    /// meantime, in which case no session should start.
    pub fn begin(&self, serial: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.stopped.contains(serial) {
            return false;
        }
        state.active = Some(serial.to_string());
        self.session_stop.store(false, Ordering::Relaxed);
        true
    }

    /// Mark the session as over. True if it ended because of `stop`.
    pub fn end(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let serial = state.active.take();
        serial.is_some_and(|s| state.stopped.contains(&s))
    }

    fn handle(&self, request: Request) -> String {
        let mut state = self.state.lock().unwrap();
        match request {
            Request::Stop => {
                let mut targets: BTreeSet<String> = state.ready.iter().cloned().collect();
                targets.extend(state.active.clone());
                targets.retain(|s| !state.stopped.contains(s));
                if targets.is_empty() {
                    return if state.stopped.is_empty() {
                        "no device connected; nothing to stop".to_string()
                    } else {
                        "already stopped".to_string()
                    };
                }
                if state.active.as_ref().is_some_and(|s| targets.contains(s)) {
                    self.session_stop.store(true, Ordering::Relaxed);
                }
                let list = targets.iter().cloned().collect::<Vec<_>>().join(", ");
                tracing::info!("stopped by command: {list}");
                state.stopped.extend(targets);
                format!(
                    "stopped {list}\n\
                     it stays stopped until it is reconnected or `moreland start` is run"
                )
            }
            Request::Start => {
                if state.stopped.is_empty() {
                    return "not stopped; nothing to start".to_string();
                }
                let list = std::mem::take(&mut state.stopped)
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::info!("started by command: {list}");
                format!("starting {list}")
            }
            Request::Status => {
                let mut lines = Vec::new();
                if let Some(active) = &state.active {
                    if !state.stopped.contains(active) {
                        lines.push(format!("streaming   {active}"));
                    }
                }
                for serial in &state.stopped {
                    lines.push(format!(
                        "stopped     {serial}  (until reconnected or `moreland start`)"
                    ));
                }
                for serial in &state.ready {
                    if state.active.as_ref() != Some(serial) && !state.stopped.contains(serial) {
                        lines.push(format!("connected   {serial}"));
                    }
                }
                if lines.is_empty() {
                    lines.push("running; no device connected".to_string());
                }
                lines.join("\n")
            }
        }
    }
}

/// The listening socket. Removes its file when dropped.
pub struct Server {
    path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind the control socket and answer requests on a background thread.
///
/// Fails if another daemon already answers on it: two daemons would fight
/// over the same tablet.
pub fn serve(control: Arc<Control>) -> Result<Server> {
    let path = socket_path();
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!(
                "another moreland daemon is already running (control socket {})",
                path.display()
            );
        }
        // Left behind by a daemon that did not exit cleanly.
        std::fs::remove_file(&path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    // Matters for the /tmp fallback; $XDG_RUNTIME_DIR is private already.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    if let Err(e) = answer(&control, stream) {
                        tracing::debug!("control request failed: {e:#}");
                    }
                }
                Err(e) => tracing::warn!("control socket: {e}"),
            }
        }
    });
    Ok(Server { path })
}

fn answer(control: &Control, mut stream: UnixStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = String::new();
    BufReader::new((&stream).take(64)).read_line(&mut line)?;
    let reply = match Request::parse(line.trim()) {
        Some(request) => control.handle(request),
        None => format!("unknown request {:?}", line.trim()),
    };
    stream.write_all(reply.as_bytes())?;
    Ok(())
}

/// The client side: send `request` to the running daemon and return its reply.
pub fn send(request: Request) -> Result<String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        anyhow::anyhow!(
            "no moreland daemon is running ({}: {e}).\n\
             Start it with `systemctl --user start moreland` or by running `moreland`.",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{}", request.word())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(serial: &str, state: &str) -> Device {
        Device {
            serial: serial.to_string(),
            state: state.to_string(),
        }
    }

    fn control() -> (Control, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        (Control::new(Arc::clone(&flag)), flag)
    }

    #[test]
    fn stop_ends_the_session_and_holds_until_unplugged() {
        let (control, flag) = control();
        control.observe(&[device("A", "device")]);
        assert!(control.begin("A"));

        control.handle(Request::Stop);
        assert!(
            flag.load(Ordering::Relaxed),
            "the session was not told to stop"
        );
        assert!(control.end(), "the session end was not attributed to stop");
        assert!(control.is_stopped("A"));

        // Still plugged in, even as it flips through other states.
        control.observe(&[device("A", "offline")]);
        control.observe(&[device("A", "device")]);
        assert!(control.is_stopped("A"));
        assert!(!control.begin("A"));

        // Unplugged, then plugged back in.
        control.observe(&[]);
        control.observe(&[device("A", "device")]);
        assert!(!control.is_stopped("A"));
        assert!(control.begin("A"));
        assert!(
            !flag.load(Ordering::Relaxed),
            "a new session inherited the stop"
        );
    }

    #[test]
    fn start_lifts_the_stop_without_reconnecting() {
        let (control, _) = control();
        control.observe(&[device("A", "device")]);
        control.handle(Request::Stop);
        assert!(control.is_stopped("A"));
        control.handle(Request::Start);
        assert!(!control.is_stopped("A"));
    }

    #[test]
    fn stop_covers_a_device_between_sessions() {
        let (control, flag) = control();
        control.observe(&[device("A", "device"), device("B", "unauthorized")]);
        control.handle(Request::Stop);
        assert!(control.is_stopped("A"));
        assert!(!control.is_stopped("B"));
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn a_session_that_ends_on_its_own_is_not_a_stop() {
        let (control, _) = control();
        control.observe(&[device("A", "device")]);
        assert!(control.begin("A"));
        assert!(!control.end());
        assert!(!control.is_stopped("A"));
    }

    #[test]
    fn requests_round_trip_over_the_socket() {
        let dir = std::env::temp_dir().join(format!("moreland-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // No other test in this binary reads XDG_RUNTIME_DIR.
        std::env::set_var("XDG_RUNTIME_DIR", &dir);

        let (control, _) = control();
        let control = Arc::new(control);
        control.observe(&[device("A", "device")]);
        let server = serve(Arc::clone(&control)).unwrap();
        assert!(
            serve(Arc::clone(&control)).is_err(),
            "a second daemon was allowed"
        );

        assert!(send(Request::Stop).unwrap().starts_with("stopped A"));
        assert!(send(Request::Status).unwrap().contains("stopped     A"));
        assert_eq!(send(Request::Start).unwrap(), "starting A");

        drop(server);
        assert!(!socket_path().exists());
        assert!(send(Request::Status).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
