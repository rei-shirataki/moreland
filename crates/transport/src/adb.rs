// SPDX-License-Identifier: Apache-2.0

//! Thin wrapper over the `adb` CLI.

use crate::process::run_with_timeout;
use anyhow::{bail, Context, Result};
use std::process::Command;

/// Override the adb binary with `MORELAND_ADB` when it is not on `PATH`.
fn adb_binary() -> String {
    std::env::var("MORELAND_ADB").unwrap_or_else(|_| "adb".to_string())
}

fn adb(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(adb_binary());
    cmd.args(args);
    let output = run_with_timeout(&mut cmd, std::time::Duration::from_secs(10))
        .with_context(|| format!("running `adb {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "adb {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug, Clone)]
pub struct Device {
    pub serial: String,
    pub model: Option<String>,
}

/// Devices in the `device` state. Ignores `unauthorized` and `offline`, which
/// cannot be forwarded to.
pub fn devices() -> Result<Vec<Device>> {
    let out = adb(&["devices", "-l"])?;
    let mut devices = Vec::new();
    for line in out.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(serial), Some(state)) = (fields.next(), fields.next()) else {
            continue;
        };
        if state != "device" {
            continue;
        }
        let model = fields
            .find(|f| f.starts_with("model:"))
            .map(|f| f.trim_start_matches("model:").to_string());
        devices.push(Device {
            serial: serial.to_string(),
            model,
        });
    }
    Ok(devices)
}

/// Whether `serial` names a device reached over Wi-Fi rather than USB.
///
/// Wireless debugging serials are either `<ip>:<port>` (`adb connect`) or an
/// mDNS name ending in `_adb-tls-connect._tcp`; USB serials are neither.
pub fn is_wireless(serial: &str) -> bool {
    serial.contains("_adb-tls-connect") || serial.contains(':')
}

/// An `adb forward` rule, removed when dropped.
///
/// The remote spec is a `localabstract:` socket in production so traffic never
/// touches Android's TCP stack or `netd`; `tcp:` is used for host-side testing
/// before the app exists.
pub struct Forward {
    serial: String,
    local: String,
}

impl Forward {
    /// Forward `local_port` to `remote`. Port 0 lets adb pick a free one,
    /// which is what the daemon uses: a fixed port collides between two
    /// devices or two daemons, and the second silently takes over the first's
    /// forward. Read the result back with [`Forward::local_port`].
    pub fn new(serial: &str, local_port: u16, remote: &str) -> Result<Self> {
        let requested = format!("tcp:{local_port}");
        if local_port != 0 {
            // Clear any stale rule from a previous run before installing ours.
            let _ = adb(&["-s", serial, "forward", "--remove", &requested]);
        }
        let out = adb(&["-s", serial, "forward", &requested, remote])
            .with_context(|| format!("forwarding {requested} -> {remote}"))?;
        let local = if local_port == 0 {
            format!("tcp:{}", allocated_port(&out)?)
        } else {
            requested
        };
        Ok(Self {
            serial: serial.to_string(),
            local,
        })
    }

    pub fn local_port(&self) -> Result<u16> {
        self.local
            .trim_start_matches("tcp:")
            .parse()
            .context("parsing forwarded port")
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        let _ = adb(&["-s", &self.serial, "forward", "--remove", &self.local]);
    }
}

/// `adb forward tcp:0 …` prints the port it allocated on stdout.
fn allocated_port(out: &str) -> Result<u16> {
    let port: u16 = out
        .trim()
        .parse()
        .with_context(|| format!("adb did not report an allocated port: {out:?}"))?;
    if port == 0 {
        bail!("adb reported port 0 for a dynamic forward");
    }
    Ok(port)
}

pub fn shell(serial: &str, command: &str) -> Result<String> {
    adb(&["-s", serial, "shell", command])
}

/// The device's panel size in its natural orientation, as `wm size` reports it.
///
/// A tablet's natural orientation is usually portrait (e.g. `1800x2880`).
fn parse_wm_size_natural(out: &str) -> Result<(u32, u32)> {
    let parse = |prefix: &str| -> Option<(u32, u32)> {
        let line = out.lines().find(|l| l.trim().starts_with(prefix))?;
        let spec = line.split(':').nth(1)?.trim();
        let (w, h) = spec.split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    };

    let (w, h) = parse("Override size")
        .or_else(|| parse("Physical size"))
        .with_context(|| format!("could not parse `wm size` output: {out:?}"))?;

    if w == 0 || h == 0 {
        bail!("device reported a zero display dimension: {w}x{h}");
    }
    Ok((w, h))
}

/// The device's physical panel size, in **landscape** orientation.
///
/// `wm size` reports the panel as the hardware sees it, which on a tablet is
/// portrait. The virtual output is landscape unless the device says
/// otherwise, so the axes are swapped here rather than at every call site.
pub fn parse_wm_size(out: &str) -> Result<(u32, u32)> {
    let (w, h) = parse_wm_size_natural(out)?;
    Ok(if w >= h { (w, h) } else { (h, w) })
}

pub fn display_size(serial: &str) -> Result<(u32, u32)> {
    let out = shell(serial, "wm size").context("querying device display size")?;
    parse_wm_size(&out)
}

/// The display's current rotation (0..=3) from `dumpsys window displays`.
///
/// Odd values are a quarter turn from the natural orientation. Accepts both
/// `mCurRotation=1` and the symbolic `mCurRotation=ROTATION_90`.
pub fn parse_rotation(out: &str) -> Option<u32> {
    let rest = out.split("mCurRotation=").nth(1)?;
    let (rest, degrees) = match rest.strip_prefix("ROTATION_") {
        Some(symbolic) => (symbolic, true),
        None => (rest, false),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: u32 = digits.parse().ok()?;
    // The symbolic form names degrees, the numeric form a quarter-turn count.
    Some(if degrees { n / 90 } else { n } % 4)
}

/// The display's size as the app will see it right now, rotation included.
///
/// The app follows the device's rotation and reports its size once it is
/// running, which used to be the first the host heard of a portrait tablet:
/// the output was built landscape, then torn down and rebuilt. Asking up
/// front lets the first pipeline be the right one. `None` when the rotation
/// cannot be read, in which case the caller falls back to [`display_size`]
/// and the device's own report still corrects it later.
pub fn current_display_size(serial: &str) -> Option<(u32, u32)> {
    let natural = parse_wm_size_natural(&shell(serial, "wm size").ok()?).ok()?;
    let rotation = parse_rotation(&shell(serial, "dumpsys window displays").ok()?)?;
    Some(if rotation % 2 == 1 {
        (natural.1, natural.0)
    } else {
        natural
    })
}

/// Pick a streaming resolution matching the device's aspect ratio.
///
/// Streaming a tablet's native panel resolution is usually the wrong default:
/// a 2880x1800 panel is 2.5x the pixels of 1080p, which pushes the encoder hard
/// for detail no one can resolve on an 11" screen at arm's length. Scaling to
/// fit `max_width` while preserving aspect keeps the upscale on the device
/// clean and the encoder comfortable.
///
/// Both axes are rounded to even numbers, which is all NV12 chroma subsampling
/// requires. Aligning to 16 instead would be tempting for macroblock tidiness,
/// but it turns the extremely common 1080 into 1088 and breaks 16:9 exactly
/// where it matters — H.264 pads to macroblocks internally and signals the crop
/// in the SPS, so this is the encoder's problem, not ours.
///
/// `max_width` bounds the **longer** edge, so a portrait panel is capped the
/// same as its landscape orientation.
pub fn stream_resolution(panel: (u32, u32), max_width: u32) -> (u32, u32) {
    let (pw, ph) = panel;
    let long = pw.max(ph);
    let scale = |v: u32| -> u32 {
        if long <= max_width {
            v
        } else {
            ((u64::from(v) * u64::from(max_width)) / u64::from(long)) as u32
        }
    };
    (align2(scale(pw).max(2)), align2(scale(ph).max(2)))
}

fn align2(value: u32) -> u32 {
    value & !1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_port_adb_allocated() {
        assert_eq!(allocated_port("41235\n").unwrap(), 41235);
        assert!(allocated_port("").is_err());
        assert!(allocated_port("0\n").is_err());
        assert!(allocated_port("error: closed").is_err());
    }

    #[test]
    fn scales_16_10_panel_cleanly() {
        // Xiaomi Pad 6: 2880x1800 -> exactly 1920x1200, a clean 1.5x upscale.
        assert_eq!(stream_resolution((2880, 1800), 1920), (1920, 1200));
    }

    #[test]
    fn portrait_is_the_landscape_result_turned() {
        assert_eq!(stream_resolution((1800, 2880), 1920), (1200, 1920));
        assert_eq!(stream_resolution((800, 1280), 1920), (800, 1280));
    }

    #[test]
    fn leaves_smaller_panels_alone() {
        assert_eq!(stream_resolution((1920, 1080), 1920), (1920, 1080));
        assert_eq!(stream_resolution((1280, 800), 1920), (1280, 800));
    }

    #[test]
    fn preserves_16_9_exactly() {
        // 1080 must survive intact; rounding it up to 1088 would letterbox
        // every 16:9 tablet.
        assert_eq!(stream_resolution((2560, 1440), 1920), (1920, 1080));
        assert_eq!(stream_resolution((3840, 2160), 1920), (1920, 1080));
    }

    #[test]
    fn output_is_always_even() {
        for panel in [(2000, 1234), (1234, 999), (3000, 2001), (2160, 1080)] {
            let (w, h) = stream_resolution(panel, 1920);
            assert_eq!(w % 2, 0, "width {w} is odd");
            assert_eq!(h % 2, 0, "height {h} is odd");
        }
    }

    #[test]
    fn aspect_ratio_is_preserved_closely() {
        for panel in [(2880, 1800), (2560, 1440), (2000, 1234), (2160, 1620)] {
            let (w, h) = stream_resolution(panel, 1920);
            let want = f64::from(panel.0) / f64::from(panel.1);
            let got = f64::from(w) / f64::from(h);
            assert!(
                (want - got).abs() / want < 0.01,
                "aspect drift for {panel:?}: wanted {want:.4}, got {got:.4}"
            );
        }
    }

    #[test]
    fn tells_wireless_serials_from_usb() {
        assert!(is_wireless("adb-V4QBB20507200467-4Rxq8Y._adb-tls-connect._tcp"));
        assert!(is_wireless("192.168.0.5:5555"));
        assert!(!is_wireless("V4QBB20507200467"));
    }

    #[test]
    fn parses_rotation_numeric_and_symbolic() {
        assert_eq!(parse_rotation("    mCurRotation=0 mOriginalRotation=1\n"), Some(0));
        assert_eq!(parse_rotation("mCurRotation=1 x"), Some(1));
        assert_eq!(parse_rotation("mCurRotation=ROTATION_90 x"), Some(1));
        assert_eq!(parse_rotation("mCurRotation=ROTATION_270"), Some(3));
        assert_eq!(parse_rotation("nothing here"), None);
    }

    #[test]
    fn parses_wm_size_physical_and_override() {
        assert_eq!(
            parse_wm_size("Physical size: 1800x2880\n").unwrap(),
            (2880, 1800)
        );
        assert_eq!(
            parse_wm_size("Physical size: 1800x2880\nOverride size: 1440x2304\n").unwrap(),
            (2304, 1440)
        );
        assert!(parse_wm_size("Physical size: 0x0\n").is_err());
        assert!(parse_wm_size("garbage\n").is_err());
    }
}
