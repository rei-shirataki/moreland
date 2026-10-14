// SPDX-License-Identifier: Apache-2.0

//! Moreland daemon.
//!
//!   moreland                 watch for the tablet; stream whenever it is plugged in
//!   moreland --once          stream one session, then exit
//!   moreland --seconds N     stop after N seconds (implies --once, --stats)
//!
//! Plug the tablet in and a virtual monitor appears; unplug it and the monitor
//! disappears. No commands in between.

mod inflight;
mod output;
mod session;
mod tracker;

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracker::DeviceTracker;

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
}

/// Parse the command line, rejecting anything not understood.
///
/// Silently falling back is worse than refusing here: a typo like
/// `--bitrate=8000` or `--fps abc` used to run with a default the user never
/// chose — and `--fps abc` even marked it explicit, overriding device tuning.
/// Both `--flag value` and `--flag=value` are accepted.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut config = session::Config::default();
    let mut once = false;
    let mut seconds = None;
    let (mut width, mut height) = (None, None);

    let mut it = args.into_iter();
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
        Err(e) => {
            eprintln!("moreland: {e}\nTry `moreland --help`.");
            std::process::exit(2);
        }
    };

    // Refuse to start rather than discovering this once per device event. The
    // compositor cannot change under a running daemon — the unit is
    // PartOf=graphical-session.target — so an unsupported one is fatal, not
    // transient, and the retry backoff would otherwise spin on it for as long
    // as the session lasts.
    let compositor = output::Compositor::detect();
    compositor.ensure_supported()?;
    tracing::info!("compositor: {}", compositor.name());

    let shutdown = Arc::new(AtomicBool::new(false));

    // SIGTERM matters here: systemd uses it to stop the service, and the
    // virtual output must come down cleanly or Hyprland is left with a
    // phantom monitor.
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
    }

    if let Some(seconds) = args.seconds {
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            shutdown.store(true, Ordering::Relaxed);
        });
    }

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

    while !shutdown.load(Ordering::Relaxed) {
        // Short timeout: absence of an update is normal and just means the
        // list still stands.
        tracker.set_timeout(Some(Duration::from_millis(500)))?;
        match tracker.next_update() {
            Ok(update) => {
                devices = update;
                announced_idle = false;
                consecutive_failures = 0;
            }
            Err(e) if is_timeout(&e) => {}
            Err(e) => {
                tracing::warn!("device tracker lost: {e}; reconnecting");
                devices.clear();
                std::thread::sleep(Duration::from_secs(2));
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

        let Some(device) = devices.iter().find(|d| d.is_ready()).cloned() else {
            if !announced_idle {
                if devices.is_empty() {
                    tracing::info!("no device connected");
                } else {
                    for device in &devices {
                        tracing::info!("device {} is {} — waiting", device.serial, device.state);
                    }
                }
                announced_idle = true;
            }
            continue;
        };

        tracing::info!("device {} connected", device.serial);
        let outcome = session::run(&device.serial, &args.config, &shutdown);

        if args.once || shutdown.load(Ordering::Relaxed) {
            match outcome {
                Ok(()) => tracing::info!("session ended cleanly"),
                Err(e) => tracing::warn!("session ended: {e:#}"),
            }
            break;
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
        std::thread::sleep(backoff);
    }

    tracing::info!("shutting down");
    Ok(())
}

fn is_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
    })
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
    fn help_is_recognised() {
        assert!(matches!(parse(&["-h"]), Ok(Command::Help)));
    }
}
