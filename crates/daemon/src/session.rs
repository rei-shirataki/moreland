// SPDX-License-Identifier: Apache-2.0

//! One streaming session: virtual output -> capture -> encode -> USB -> tablet.

use anyhow::{bail, Context, Result};
use capture::session::{BufferMode, Capture, CaptureConfig};
use encoder::{Encoder, EncoderConfig};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use transport::{adb, stream_header, Sender};

use crate::inflight::InFlight;
use crate::output::VirtualOutput;
use crate::touch::{self, TouchInjector};

const APP_PACKAGE: &str = "com.moreland.display";
const APP_ACTIVITY: &str = "com.moreland.display/.DisplayActivity";

/// Frames allowed in flight (sent, not yet rendered and acknowledged).
///
/// Without a bound the host sends as fast as the compositor produces, and a
/// decoder slower than that — the Kirin 659's MSVDX is — builds a standing
/// queue in the socket and adb forward: a full-screen animation measured a
/// 285 ms median round trip against 11 ms at best. Capping the pipeline and
/// dropping the surplus *before* encoding keeps latency at roughly this many
/// frame times instead, at the cost of showing fewer frames.
const DEFAULT_MAX_IN_FLIGHT: usize = 3;

/// With no acknowledgement for this long, assume the device is not rendering
/// (surface detached, app backgrounded) and stop gating on it. Frames are
/// acked only when rendered, so without this the window would never reopen.
const ACK_STALL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub struct Config {
    /// Explicit resolution. `None` matches the device's own panel aspect ratio.
    pub resolution: Option<(u32, u32)>,
    /// Cap on the auto-detected width, to keep encoder load sane.
    pub max_width: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Unacknowledged frames allowed in flight; see `DEFAULT_MAX_IN_FLIGHT`.
    pub max_in_flight: usize,
    /// CABAC entropy coding. CAVLC is cheaper for a weak decoder to parse.
    pub cabac: bool,
    /// Encoder speed/quality balance, 1 (quality) .. 7 (speed).
    pub target_usage: u32,
    /// Set when the user passed `--fps` / `--bitrate`; device tuning never
    /// overrides an explicit choice.
    pub fps_explicit: bool,
    pub bitrate_explicit: bool,
    pub max_in_flight_explicit: bool,
    pub position_x: i32,
    pub position_y: i32,
    pub output_name: String,
    /// Composite the mouse cursor into the streamed frames.
    pub paint_cursor: bool,
    /// Replay the tablet's touches on the host through uinput.
    pub touch: bool,
    /// Emit round-trip latency statistics on exit.
    pub stats: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            resolution: None,
            max_width: 1920,
            // Measured on Renoir + Pad 6 at 1920x1200, once capture and encode
            // were on the same GPU: median round trip 33.5 ms at 60, 25.5 ms at
            // 90, 18.8 ms at 120. Higher rates *lower* latency (a shorter frame
            // interval means less waiting for the next slot) and cost fewer
            // bytes, since smaller inter-frame deltas compress better. 144
            // saturates: 163 of 1764 frames unacked and a 208 ms stall.
            fps: 90,
            bitrate_kbps: 20_000,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            cabac: true,
            target_usage: 3,
            fps_explicit: false,
            bitrate_explicit: false,
            max_in_flight_explicit: false,
            // Below a primary at 0x0, starting at y = 1080. This assumes the
            // primary is no taller than 1080px — on a taller display the
            // virtual output would overlap it rather than sit below. Override
            // with `--position`/`--position-y` if that does not hold for your
            // layout.
            position_x: 0,
            position_y: 1080,
            output_name: capture::VIRTUAL_OUTPUT_NAME.to_string(),
            paint_cursor: false,
            touch: true,
            stats: false,
        }
    }
}

impl Config {
    /// Apply per-device defaults for hardware known to need them.
    ///
    /// The 90 fps default was measured on a Qualcomm tablet. HiSilicon
    /// Kirin 659 (`hi6250`) decodes through an IMG MSVDX block that rejects
    /// `KEY_LOW_LATENCY` and is built for 1080p-class streams, so 1920x1200 at
    /// 90 fps backs up its input queue and latency climbs instead of falling.
    fn tuned_for(&self, serial: &str) -> Config {
        let mut tuned = self.clone();
        let platform = adb::shell(serial, "getprop ro.board.platform").unwrap_or_default();
        if platform.trim() == "hi6250" {
            if !tuned.fps_explicit {
                tuned.fps = 60;
            }
            if !tuned.bitrate_explicit {
                tuned.bitrate_kbps = 12_000;
            }
            // Measured with a full-screen animation: the MSVDX decoder tops
            // out near 60 fps whatever the offered rate (90 fps delivered the
            // same ~1135 frames and only added latency), so latency tracks
            // the in-flight depth. 2 keeps every frame the decoder can take at
            // an 18 ms median; 3 gives 34 ms, 1 halves the delivered frames.
            // Bitrate, CAVLC and target-usage made no measurable difference.
            if !tuned.max_in_flight_explicit {
                tuned.max_in_flight = 2;
            }
            tracing::info!(
                "device profile hi6250 (Kirin 659): {} fps, {} kbps, {} in flight",
                tuned.fps,
                tuned.bitrate_kbps,
                tuned.max_in_flight
            );
        }
        tuned
    }
}

/// `moreland-` and 128 random bits in hex: unguessable, and safe to pass
/// through `adb shell` unquoted.
fn session_socket_name() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom for the session socket name")?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("moreland-{hex}"))
}

pub fn app_installed(serial: &str) -> Result<bool> {
    let out = adb::shell(serial, &format!("pm list packages {APP_PACKAGE}"))?;
    Ok(out.contains(APP_PACKAGE))
}

/// Stream until `shutdown` is set, the device vanishes, or the app disconnects.
///
/// Every resource here is RAII-scoped: the virtual output and the adb forward
/// are removed when this function returns, however it returns.
pub fn run(serial: &str, config: &Config, shutdown: &AtomicBool) -> Result<()> {
    let config = &config.tuned_for(serial);
    if !app_installed(serial)? {
        bail!(
            "app not installed on the tablet.\n\
             Build it with `cd android && ANDROID_HOME=/opt/android-sdk ./gradlew assembleRelease`,\n\
             then `adb install -r android/app/build/outputs/apk/release/app-release.apk`."
        );
    }

    // Match the tablet's own aspect ratio unless told otherwise, so the image
    // fills its screen instead of letterboxing.
    let (width, height) = match config.resolution {
        Some(explicit) => explicit,
        None => {
            let panel = adb::display_size(serial)?;
            let scaled = adb::stream_resolution(panel, config.max_width);
            tracing::info!(
                "device panel {}x{} (landscape) -> streaming {}x{}",
                panel.0,
                panel.1,
                scaled.0,
                scaled.1
            );
            scaled
        }
    };

    let output = VirtualOutput::create(
        &config.output_name,
        width,
        height,
        config.fps,
        config.position_x,
        config.position_y,
    )?;
    if output.applied_mode() {
        tracing::info!(
            "virtual output {} at {}x{}@{}",
            output.name(),
            width,
            height,
            config.fps
        );
    } else {
        tracing::info!(
            "using existing output {} with its own mode; encoding at whatever \
             size it reports",
            output.name()
        );
    }

    // A fresh name per session. Abstract socket names are first-come on
    // Android, so with a fixed one any app on the tablet could bind it while
    // ours is force-stopped below and receive the desktop stream instead.
    let socket = session_socket_name()?;
    // Port 0: adb picks a free one, so two devices or daemons never collide.
    let forward = adb::Forward::new(serial, 0, &format!("localabstract:{socket}"))?;
    let port = forward.local_port()?;

    // The abstract socket name is process-wide on Android, so a stale instance
    // from a previous run would keep it bound and answer with a dead surface.
    let _ = adb::shell(serial, &format!("am force-stop {APP_PACKAGE}"));
    std::thread::sleep(Duration::from_millis(500));

    // Wake the tablet first. A sleeping or locked device starts the activity
    // without ever making it visible, so no surface is created and the session
    // dies with "no valid surface available" — which looks like a crash but is
    // just a dark screen. The manifest's showWhenLocked/turnScreenOn only help
    // once the activity is actually being brought up.
    let _ = adb::shell(serial, "input keyevent KEYCODE_WAKEUP");
    let _ = adb::shell(serial, "wm dismiss-keyguard");
    std::thread::sleep(Duration::from_millis(400));

    adb::shell(
        serial,
        &format!(
            "am start -n {APP_ACTIVITY} --es {} {socket}",
            protocol::SOCKET_EXTRA
        ),
    )
        .context("launching the display app")?;
    std::thread::sleep(Duration::from_millis(2500));

    // Probe what this machine's encoder can actually import rather than
    // assuming; both the format and its modifiers are GPU-vendor specific.
    // XR24 first — the output is opaque, so alpha is wasted work — then AR24,
    // which is all Intel's iHD driver offers for 8-bit RGB (issue #3).
    let mut allowed: Vec<(u32, Vec<u64>)> = [capture::XR24, capture::AR24]
        .into_iter()
        .filter_map(|f| encoder::supported_modifiers(f).map(|m| (f, m)))
        .collect();
    // Formats the encoder cannot import as DMA-BUF stay usable as a last
    // resort: the encoder wraps a linear buffer as system memory instead
    // (see `Encoder::new`). Listed after everything zero-copy, so they are
    // only picked when the compositor offers nothing better.
    for f in [capture::XR24, capture::AR24] {
        if !allowed.iter().any(|(a, _)| *a == f) {
            allowed.push((f, vec![0]));
        }
    }
    tracing::debug!(
        "consumer accepts {:02x?}",
        allowed
            .iter()
            .map(|(f, m)| (capture::session::fourcc_name(*f), m))
            .collect::<Vec<_>>()
    );

    let mut capture = Capture::new(
        output.name(),
        &CaptureConfig {
            mode: BufferMode::Dmabuf,
            pool_size: 3,
            allowed_formats: allowed,
            paint_cursor: config.paint_cursor,
        },
    )?;
    tracing::info!(
        "capture format {} modifier 0x{:016x}",
        capture::session::fourcc_name(capture.format),
        capture.modifier.unwrap_or(0)
    );
    let encoder = Arc::new(Encoder::new(&EncoderConfig {
        width: capture.width,
        height: capture.height,
        framerate: config.fps,
        bitrate_kbps: config.bitrate_kbps,
        cabac: config.cabac,
        target_usage: config.target_usage,
        fourcc: capture.format,
        modifier: capture.modifier.unwrap_or(0),
        ..Default::default()
    })?);

    let mut sender = Sender::connect(
        port,
        &stream_header(capture.width, capture.height, config.fps)?,
    )
    .context("connecting to the app — is it in the foreground on the tablet?")?;
    tracing::info!("streaming to {serial}");

    let running = Arc::new(AtomicBool::new(true));
    let in_flight: Arc<Mutex<InFlight>> = Arc::new(Mutex::new(InFlight::default()));
    let round_trips: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let last_ack: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));

    // Created after the output exists so the compositor can bind the device to
    // it. Touch is optional: without /dev/uinput access the display still works.
    let mut touch_in = if config.touch {
        match TouchInjector::create() {
            Ok(injector) => {
                // Hyprland discovers the device asynchronously; give it a
                // moment before naming it in a rule.
                std::thread::sleep(Duration::from_millis(300));
                if let Err(e) = output.bind_input_device(touch::DEVICE_NAME) {
                    tracing::warn!(
                        "touch is on, but could not be bound to {}: {e:#}\n\
                         map {} to that output in your compositor config",
                        output.name(),
                        touch::DEVICE_NAME
                    );
                }
                Some(injector)
            }
            Err(e) => {
                tracing::warn!("touch disabled: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let ack_thread = {
        let last_ack = Arc::clone(&last_ack);
        let mut reader = sender.ack_reader()?;
        let in_flight = Arc::clone(&in_flight);
        let round_trips = Arc::clone(&round_trips);
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            // Messages are small and fixed-length per tag; a reply can arrive
            // split across reads, so keep what has not parsed yet.
            let mut pending: Vec<u8> = Vec::with_capacity(256);
            let mut chunk = [0u8; 256];
            'read: while running.load(Ordering::Relaxed) {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        pending.extend_from_slice(&chunk[..n]);
                        let mut consumed = 0usize;
                        loop {
                            match protocol::parse_device_message(&pending[consumed..]) {
                                Ok(Some((message, used))) => {
                                    consumed += used;
                                    match message {
                                        protocol::DeviceMessage::Ack { pts_ns } => {
                                            let now = Instant::now();
                                            *last_ack.lock().unwrap() = now;
                                            let rtt = in_flight.lock().unwrap().ack(pts_ns, now);
                                            if let Some(rtt) = rtt {
                                                round_trips.lock().unwrap().push(rtt);
                                            }
                                        }
                                        protocol::DeviceMessage::Touch(event) => {
                                            if let Some(injector) = touch_in.as_mut() {
                                                if let Err(e) = injector.inject(event) {
                                                    tracing::warn!("touch injection failed: {e:#}");
                                                    touch_in = None;
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    // No framing to resynchronise on.
                                    tracing::warn!("device sent garbage, closing: {e:#}");
                                    break 'read;
                                }
                            }
                        }
                        pending.drain(..consumed);
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        })
    };

    // The drain thread owns the sender: an encoded packet goes out the moment
    // it exists. Sending from the capture loop instead held every packet until
    // the *next* capture returned, which on a static screen is an arbitrarily
    // long wait and on a busy one is up to a full frame of added latency.
    let drain_thread = {
        let encoder = Arc::clone(&encoder);
        let running = Arc::clone(&running);
        let in_flight = Arc::clone(&in_flight);
        std::thread::spawn(move || -> (Sender, Result<()>) {
            while running.load(Ordering::Relaxed) {
                match encoder.pull_packet(Duration::from_millis(100)) {
                    Ok(Some(p)) => {
                        in_flight.lock().unwrap().push(p.pts_ns, Instant::now());
                        if let Err(e) = sender.send_frame(&p.data, p.pts_ns, p.keyframe) {
                            running.store(false, Ordering::Relaxed);
                            return (sender, Err(e.into()));
                        }
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::error!("encoder: {e}");
                        running.store(false, Ordering::Relaxed);
                        return (sender, Err(e));
                    }
                }
            }
            (sender, Ok(()))
        })
    };

    let frame_duration_ns = 1_000_000_000u64 / u64::from(config.fps);
    let mut index = 0u64;
    let mut dropped = 0u64;
    let result = (|| -> Result<()> {
        while !shutdown.load(Ordering::Relaxed) && running.load(Ordering::Relaxed) {
            let timing = capture.capture_frame()?;
            let slot = index;
            index += 1;

            // Shed load before encoding: a dropped source frame costs nothing,
            // while one already queued toward a slow decoder is latency.
            {
                let mut in_flight = in_flight.lock().unwrap();
                if in_flight.len() >= config.max_in_flight {
                    if last_ack.lock().unwrap().elapsed() < ACK_STALL {
                        dropped += 1;
                        continue;
                    }
                    // Nothing has been acknowledged for a while; forget the
                    // stale entries so a recovering device is not gated on
                    // frames that will never be acked.
                    in_flight.clear();
                }
            }
            let dmabuf = capture
                .dmabuf(timing.buffer_index)
                .context("capture produced no DMA-BUF")?;
            encoder.push_frame(dmabuf, slot * frame_duration_ns)?;
        }
        Ok(())
    })();

    running.store(false, Ordering::Relaxed);
    let (sender, send_result) = drain_thread
        .join()
        .map_err(|_| anyhow::anyhow!("send thread panicked"))?;
    let _ = ack_thread.join();

    if config.stats {
        report(
            &round_trips.lock().unwrap(),
            index,
            dropped,
            config.max_in_flight,
            sender.bytes_sent(),
        );
    }

    // Leaving the app in the foreground with a dead socket is confusing; send
    // it home so the tablet returns to a normal state.
    let _ = adb::shell(serial, &format!("am force-stop {APP_PACKAGE}"));

    drop(forward);
    drop(output);
    result.and(send_result)
}

fn report(trips: &[Duration], frames: u64, dropped: u64, max_in_flight: usize, bytes: u64) {
    println!("\n  frames captured   {frames}");
    println!("  frames dropped    {dropped}  (kept under {max_in_flight} in flight)");
    println!("  bytes sent        {:.1} MB", bytes as f64 / 1e6);
    println!("  acks received     {}", trips.len());
    if trips.is_empty() {
        println!("\n  no acknowledgements — check `adb logcat -s Moreland`");
        return;
    }
    let mut trips = trips.to_vec();
    trips.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!("\n  round trip: host send -> device render -> host ack");
    println!("    min       {:>8.2} ms", ms(trips[0]));
    println!("    median    {:>8.2} ms", ms(trips[trips.len() / 2]));
    println!("    p95       {:>8.2} ms", ms(trips[trips.len() * 95 / 100]));
    println!("    max       {:>8.2} ms", ms(trips[trips.len() - 1]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_socket_names_are_fresh_and_shell_safe() {
        let a = session_socket_name().unwrap();
        let b = session_socket_name().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), "moreland-".len() + 32);
        // Must also satisfy the app's own pattern, [A-Za-z0-9._-]{1,64}.
        assert!(a.len() <= 64);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)));
    }
}
