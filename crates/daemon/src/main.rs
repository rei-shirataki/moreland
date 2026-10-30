// SPDX-License-Identifier: Apache-2.0

//! Moreland daemon.
//!
//!   moreland                 watch for the tablet; stream whenever it is plugged in
//!   moreland --once          stream one session, then exit
//!   moreland --seconds N     stop after N seconds (implies --once, --stats)
//!   moreland stop|start|status
//!                            control the running daemon; see `control`
//!
//! Plug the tablet in and a virtual monitor appears; unplug it and the monitor
//! disappears. No commands in between.

mod control;
mod inflight;
mod output;
mod session;
mod touch;
mod tracker;

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracker::DeviceTracker;

/// Exit statuses that restarting cannot fix. The systemd unit lists all of them in
/// `RestartPreventExitStatus=`, so it gives up instead of relaunching every
/// `RestartSec` for as long as the session lasts.
const EXIT_USAGE: i32 = 2;
const EXIT_UNSUPPORTED: i32 = 3;
/// Another daemon owns the control socket (and so the tablet).
const EXIT_ALREADY_RUNNING: i32 = 4;

#[derive(Debug)]
struct Args {
    config: session::Config,
    once: bool,
    seconds: Option<u64>,
}

/// What the command line asked for.
#[derive(Debug)]
enum Command {
    Run(Args),
    Help,
    /// Talk to the daemon that is already running.
    Control(control::Request),
}

/// Parse the command line, rejecting anything not understood.
///
/// Silently falling back is worse than refusing here: a typo like
/// `--bitrate=8000` or `--fps abc` used to run with a default the user never
/// chose — and `--fps abc` even marked it explicit, overriding device tuning.
/// Both `--flag value` and `--flag=value` are accepted.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter().peekable();
    if let Some(request) = args.peek().and_then(|a| control::Request::parse(a)) {
        let word = args.next().unwrap();
        if let Some(extra) = args.next() {
            return Err(format!(
                "`moreland {word}` takes no arguments, got {extra:?}"
            ));
        }
        return Ok(Command::Control(request));
    }

    let mut config = session::Config::default();
    let mut once = false;
    let mut seconds = None;
    let (mut width, mut height) = (None, None);

    let mut it = args;
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_string(), Some(value.to_string()))
            }
            _ => (arg.clone(), None),
        };
        let mut value = || -> Result<String, String> {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        let takes_value = matches!(
            flag.as_str(),
            "--width"
                | "--height"
                | "--max-width"
                | "--fps"
                | "--bitrate"
                | "--max-in-flight"
                | "--target-usage"
                | "--position"
                | "--position-y"
                | "--scale"
                | "--output-name"
                | "--seconds"
        );
        if !takes_value && inline.is_some() {
            return Err(format!("{flag} does not take a value"));
        }

        match flag.as_str() {
            "--width" => width = Some(number(&flag, &value()?, 1, u32::from(u16::MAX))?),
            "--height" => height = Some(number(&flag, &value()?, 1, u32::from(u16::MAX))?),
            "--max-width" => config.max_width = number(&flag, &value()?, 2, u32::MAX)?,
            "--native" => config.max_width = u32::MAX,
            "--fps" => {
                config.fps = number(&flag, &value()?, 1, 1000)?;
                config.fps_explicit = true;
            }
            "--bitrate" => {
                config.bitrate_kbps = number(&flag, &value()?, 1, u32::MAX)?;
                config.bitrate_explicit = true;
            }
            "--max-in-flight" => {
                config.max_in_flight = number(&flag, &value()?, 1, 64)? as usize;
                config.max_in_flight_explicit = true;
            }
            "--cavlc" => config.cabac = false,
            "--target-usage" => config.target_usage = number(&flag, &value()?, 1, 7)?,
            "--position" => config.position_x = number(&flag, &value()?, i32::MIN, i32::MAX)?,
            "--position-y" => config.position_y = number(&flag, &value()?, i32::MIN, i32::MAX)?,
            "--scale" => {
                let text = value()?;
                config.scale = text
                    .parse::<f64>()
                    .ok()
                    .filter(|v| (0.5..=4.0).contains(v))
                    .ok_or_else(|| format!("--scale {text:?} must be a number in 0.5..4"))?;
            }
            "--output-name" => {
                let name = value()?;
                // The name is spliced into `hyprctl keyword monitor` rules,
                // which are comma-separated.
                if name.is_empty() || name.contains(',') || name.contains(char::is_whitespace) {
                    return Err(format!(
                        "--output-name {name:?} must be non-empty, without commas or spaces"
                    ));
                }
                config.output_name = name;
            }
            "--show-cursor" => config.paint_cursor = true,
            "--no-touch" => config.touch = false,
            "--once" => once = true,
            "--stats" => config.stats = true,
            "--seconds" => {
                seconds = Some(number(&flag, &value()?, 1, u64::MAX)?);
                once = true;
                config.stats = true;
            }
            "--help" | "-h" => return Ok(Command::Help),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    // A half-specified size would silently distort the aspect ratio.
    config.resolution = match (width, height) {
        (Some(w), Some(h)) => Some((w, h)),
        (Some(_), None) | (None, Some(_)) => {
            return Err("--width and --height must be given together".to_string())
        }
        (None, None) => None,
    };

    Ok(Command::Run(Args {
        config,
        once,
        seconds,
    }))
}

fn number<T>(flag: &str, value: &str, min: T, max: T) -> Result<T, String>
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display,
{
    let parsed: T = value
        .parse()
        .map_err(|_| format!("{flag}: {value:?} is not a number"))?;
    if parsed < min || parsed > max {
        return Err(format!("{flag}: {value} is out of range ({min}..={max})"));
    }
    Ok(parsed)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Command::Run(args)) => args,
        Ok(Command::Help) => {
            println!("{}", include_str!("usage.txt"));
            return Ok(());
        }
        Ok(Command::Control(request)) => match control::send(request) {
            Ok(reply) => {
                println!("{reply}");
                return Ok(());
            }
            Err(e) => {
                eprintln!("moreland: {e:#}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("moreland: {e}\nTry `moreland --help`.");
            std::process::exit(EXIT_USAGE);
        }
    };

    // Refuse to start rather than discovering this once per device event. The
    // compositor cannot change under a running daemon — the unit is
    // PartOf=graphical-session.target — so an unsupported one is fatal, not
    // transient, and the retry backoff would otherwise spin on it for as long
    // as the session lasts.
    let compositor = output::Compositor::detect();
    if let Err(e) = compositor.ensure_supported() {
        tracing::error!("{e:#}");
        std::process::exit(EXIT_UNSUPPORTED);
    }
    tracing::info!("compositor: {}", compositor.name());

    let shutdown = Arc::new(AtomicBool::new(false));
    // Ends the current session only. `moreland stop` sets it alone; anything
    // that sets `shutdown` sets it too.
    let session_stop = Arc::new(AtomicBool::new(false));

    // SIGTERM matters here: systemd uses it to stop the service, and the
    // virtual output must come down cleanly or Hyprland is left with a
    // phantom monitor.
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
        signal_hook::flag::register(signal, Arc::clone(&session_stop))?;
    }

    if let Some(seconds) = args.seconds {
        let shutdown = Arc::clone(&shutdown);
        let session_stop = Arc::clone(&session_stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            shutdown.store(true, Ordering::Relaxed);
            session_stop.store(true, Ordering::Relaxed);
        });
    }

    let control = Arc::new(control::Control::new(Arc::clone(&session_stop)));
    // A one-off run is not the daemon and does not answer `moreland stop`.
    let _control_server = if args.once {
        None
    } else {
        match control::serve(Arc::clone(&control)) {
            Ok(server) => Some(server),
            Err(e) => {
                tracing::error!("{e:#}");
                std::process::exit(EXIT_ALREADY_RUNNING);
            }
        }
    };

    let mut tracker = DeviceTracker::connect()?;
    tracing::info!("watching for devices");

    // The last device list we were told about.
    //
    // This must persist across iterations. `next_update` only returns when the
    // list *changes*, so after a session ends for any reason other than the
    // device vanishing — a dropped socket, the app being closed, a compositor
    // hiccup — no further update is ever sent. Blocking on one there strands
    // the daemon: alive, connected, and silently never retrying.
    let mut devices: Vec<tracker::Device> = Vec::new();
    let mut consecutive_failures = 0u32;
    let mut announced_idle = false;
    // The wireless device last streamed to, and when it was last asked to
    // come back. A Wi-Fi drop removes it from the adb list for good unless
    // something reconnects it.
    let mut last_wireless: Option<String> = None;
    let mut last_reconnect = std::time::Instant::now();

    while !shutdown.load(Ordering::Relaxed) {
        // Short timeout: absence of an update is normal and just means the
        // list still stands.
        match tracker.next_update(Duration::from_millis(500)) {
            Ok(Some(update)) => {
                control.observe(&update);
                devices = update;
                announced_idle = false;
                consecutive_failures = 0;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("device tracker lost: {e}; reconnecting");
                devices.clear();
                sleep_with_shutdown(Duration::from_secs(2), &shutdown);
                match DeviceTracker::connect() {
                    Ok(t) => {
                        tracker = t;
                        tracing::info!("device tracker reconnected");
                    }
                    Err(e) => tracing::warn!("reconnect failed: {e}"),
                }
                continue;
            }
        }

        let Some(device) = devices
            .iter()
            .find(|d| d.is_ready() && !control.is_stopped(&d.serial))
            .cloned()
        else {
            if !announced_idle {
                if devices.is_empty() {
                    tracing::info!("no device connected");
                } else {
                    for device in &devices {
                        if control.is_stopped(&device.serial) {
                            tracing::info!(
                                "device {} is stopped — reconnect it or run `moreland start`",
                                device.serial
                            );
                        } else {
                            tracing::info!(
                                "device {} is {} — waiting",
                                device.serial,
                                device.state
                            );
                        }
                    }
                }
                announced_idle = true;
            }
            if let Some(serial) = last_wireless.as_deref() {
                if !control.is_stopped(serial)
                    && !devices.iter().any(|d| d.serial == serial && d.is_ready())
                    && last_reconnect.elapsed() >= WIRELESS_RECONNECT_INTERVAL
                {
                    last_reconnect = std::time::Instant::now();
                    match transport::adb::reconnect_wireless(serial) {
                        Ok(true) => tracing::debug!("asked adb to reconnect {serial}"),
                        Ok(false) => {}
                        Err(e) => tracing::debug!("wireless reconnect: {e:#}"),
                    }
                }
            }
            continue;
        };

        if !control.begin(&device.serial) {
            continue;
        }
        // `begin` cleared the session flag; a signal that landed just before
        // it must still end this session.
        if shutdown.load(Ordering::Relaxed) {
            session_stop.store(true, Ordering::Relaxed);
        }
        tracing::info!("device {} connected", device.serial);
        last_wireless = transport::adb::is_wireless(&device.serial).then(|| device.serial.clone());
        let outcome = session::run(&device.serial, &args.config, &session_stop);
        let stopped_by_command = control.end();

        if args.once || shutdown.load(Ordering::Relaxed) {
            match outcome {
                Ok(()) => tracing::info!("session ended cleanly"),
                Err(e) => tracing::warn!("session ended: {e:#}"),
            }
            break;
        }

        // Whatever the stop interrupted, it is not a failure to back off from.
        if stopped_by_command {
            if let Err(e) = outcome {
                tracing::debug!("session ended while stopping: {e:#}");
            }
            tracing::info!("session stopped by command");
            consecutive_failures = 0;
            announced_idle = false;
            continue;
        }

        match outcome {
            Ok(()) => {
                tracing::info!("session ended cleanly");
                consecutive_failures = 0;
            }
            Err(e) => {
                consecutive_failures += 1;
                tracing::warn!("session ended: {e:#}");
            }
        }

        // Back off when sessions keep failing immediately — an unplugged
        // cable, an uninstalled app, a tablet that never wakes — instead of
        // spinning on `am start` and hyprctl.
        let backoff = Duration::from_secs(2 * u64::from(consecutive_failures.min(8)).max(1));
        if consecutive_failures > 2 {
            tracing::info!("retrying in {}s", backoff.as_secs());
        }
        sleep_with_shutdown(backoff, &shutdown);
    }

    tracing::info!("shutting down");
    Ok(())
}

/// How often to ask adb to reconnect a wireless device that has dropped.
const WIRELESS_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

fn sleep_with_shutdown(duration: Duration, shutdown: &AtomicBool) {
    let start = std::time::Instant::now();
    while start.elapsed() < duration {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let remaining = duration.saturating_sub(start.elapsed());
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    fn run(args: &[&str]) -> Args {
        match parse(args) {
            Ok(Command::Run(args)) => args,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn defaults_leave_device_tuning_free() {
        let args = run(&[]);
        assert_eq!(args.config.fps, 90);
        assert!(!args.config.fps_explicit);
        assert!(args.config.resolution.is_none());
    }

    #[test]
    fn accepts_both_value_styles() {
        let args = run(&["--fps", "60", "--bitrate=8000", "--position-y=-1200"]);
        assert_eq!(args.config.fps, 60);
        assert!(args.config.fps_explicit);
        assert_eq!(args.config.bitrate_kbps, 8000);
        assert_eq!(args.config.position_y, -1200);
    }

    #[test]
    fn rejects_unknown_arguments_and_bad_values() {
        for bad in [
            &["--bitrat", "8000"][..],
            &["--fps", "abc"],
            &["--fps", "0"],
            &["--fps"],
            &["--target-usage", "9"],
            &["--width", "70000", "--height", "1200"],
            &["--width", "1920"],
            &["--once=yes"],
            &["--output-name", "a,b"],
            &["--seconds", "0"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn seconds_implies_once_and_stats() {
        let args = run(&["--seconds", "15"]);
        assert_eq!(args.seconds, Some(15));
        assert!(args.once && args.config.stats);
    }

    #[test]
    fn control_words_are_commands_only_on_their_own() {
        assert!(matches!(
            parse(&["stop"]),
            Ok(Command::Control(control::Request::Stop))
        ));
        assert!(matches!(
            parse(&["status"]),
            Ok(Command::Control(control::Request::Status))
        ));
        assert!(parse(&["stop", "--fps", "60"]).is_err());
        assert!(parse(&["--fps", "60", "start"]).is_err());
    }

    #[test]
    fn help_is_recognised() {
        assert!(matches!(parse(&["-h"]), Ok(Command::Help)));
    }

    #[test]
    fn sleep_with_shutdown_terminates_early_when_signaled() {
        let shutdown = AtomicBool::new(false);
        let start = std::time::Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                shutdown.store(true, Ordering::Relaxed);
            });
            sleep_with_shutdown(Duration::from_secs(5), &shutdown);
        });
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
