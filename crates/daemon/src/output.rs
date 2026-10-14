// SPDX-License-Identifier: Apache-2.0

//! Virtual output creation, per compositor.
//!
//! On a compositor that implements `ext-image-copy-capture-v1` this is the
//! **only** compositor-specific part of the project: capture uses that standard
//! protocol, encoding uses VA-API, and transport uses ADB, all of them
//! compositor-agnostic. Adding such a compositor means implementing one thing —
//! "create a headless output with this name and mode, and remove it later".
//!
//! A compositor *without* the protocol is a different and much larger problem,
//! and it is not solved here. KWin 6.7 and Mutter both implement no `ext-` or
//! `wlr-` capture protocol at all, so they need a second, PipeWire-based
//! capture backend before this file is even reached.
//!
//! See `docs/COMPATIBILITY.md` for what each compositor needs.

use anyhow::{bail, Context, Result};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    Hyprland,
    /// wlroots-based with a Sway-compatible IPC (`swaymsg create_output`).
    Sway,
    /// labwc, queried through `wlr-randr`. Unlike the other two it has no
    /// runtime IPC to *create* an output, so the session attaches to one the
    /// compositor was started with. See `docs/COMPATIBILITY.md`.
    Labwc,
    Unsupported,
}

impl Compositor {
    /// Identify the running compositor from its environment markers.
    ///
    /// The markers alone are not enough. A systemd user service inherits the
    /// environment of the session that started it and outlives it, so
    /// `HYPRLAND_INSTANCE_SIGNATURE` can still be set — naming an instance
    /// that exited hours ago — while the user is logged into something else
    /// entirely. Believing it there costs the honest "unsupported compositor"
    /// error and replaces it with `hyprctl ... failed:` and an empty stderr,
    /// retried on a backoff forever. So every marker is confirmed against a
    /// live IPC round-trip before it is trusted.
    pub fn detect() -> Self {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();

        if (std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
            || desktop_names(&desktop, "Hyprland"))
            && run("hyprctl", &["version"]).is_ok()
        {
            return Compositor::Hyprland;
        }
        if (std::env::var_os("SWAYSOCK").is_some() || desktop_names(&desktop, "sway"))
            && run("swaymsg", &["-t", "get_version"]).is_ok()
        {
            return Compositor::Sway;
        }
        // labwc has reported itself as both `labwc` and the generic `wlroots`
        // depending on version, so neither value alone identifies it. The
        // confirming round-trip is `wlr-randr`, which is also how this backend
        // reads output state later: if it cannot list outputs now, the backend
        // could not work anyway.
        if (desktop_names(&desktop, "labwc") || desktop_names(&desktop, "wlroots"))
            && run("wlr-randr", &[]).is_ok()
        {
            return Compositor::Labwc;
        }
        Compositor::Unsupported
    }

    pub fn name(self) -> &'static str {
        match self {
            Compositor::Hyprland => "Hyprland",
            Compositor::Sway => "Sway",
            Compositor::Labwc => "labwc",
            Compositor::Unsupported => "unsupported",
        }
    }

    /// Fail unless this compositor can host a session, so the daemon can
    /// refuse at startup instead of once per device event.
    pub fn ensure_supported(self) -> Result<()> {
        match self {
            Compositor::Hyprland => Ok(()),
            Compositor::Sway => bail!(
                "Sway support is not implemented yet.\n\
                 `swaymsg create_output` exists, but Sway names the result \
                 itself, so the daemon must diff `swaymsg -t get_outputs` to \
                 discover it. See docs/COMPATIBILITY.md."
            ),
            Compositor::Labwc => Ok(()),
            Compositor::Unsupported => {
                let desktop =
                    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unset".to_string());
                bail!(
                    "unsupported compositor (XDG_CURRENT_DESKTOP={desktop}).\n\
                     This needs a compositor that can create a headless output \
                     and implements ext-image-copy-capture-v1.\n\
                     Verified: Hyprland. KDE Plasma and GNOME implement neither \
                     and need a PipeWire capture backend first.\n\
                     Run scripts/moreland-doctor.sh for a full report, and see \
                     docs/COMPATIBILITY.md."
                )
            }
        }
    }
}

/// Whether `XDG_CURRENT_DESKTOP` names `wanted`.
///
/// The variable is a colon-separated list, most specific first — labwc sets
/// `labwc:wlroots` — so comparing the whole value misses every compositor
/// that lists more than one name.
fn desktop_names(desktop: &str, wanted: &str) -> bool {
    desktop
        .split(':')
        .any(|name| name.trim().eq_ignore_ascii_case(wanted))
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running `{program} {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `wlr-randr` lists an output by this exact name.
///
/// It prints each output name at the start of a line and indents that output's
/// properties beneath it, so the first token of a line is the name. Comparing
/// the token rather than a prefix keeps `HEADLESS-1` from matching
/// `HEADLESS-10`.
fn labwc_output_exists(name: &str) -> bool {
    run("wlr-randr", &[])
        .map(|out| {
            out.lines()
                .any(|line| line.split_whitespace().next() == Some(name))
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct MonitorState {
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    refresh_rate: f64,
    scale: f64,
    transform: i32,
}

fn hyprland_monitor_state() -> Result<Vec<MonitorState>> {
    let json =
        run("hyprctl", &["monitors", "-j"]).context("reading Hyprland monitor state")?;

    let monitors: serde_json::Value =
        serde_json::from_str(&json).context("parsing Hyprland monitor JSON")?;

    let monitors = monitors
        .as_array()
        .context("Hyprland monitor JSON is not an array")?;

    let mut states = Vec::with_capacity(monitors.len());

    for monitor in monitors {
        let name = monitor
            .get("name")
            .and_then(|v| v.as_str())
            .context("Hyprland monitor has no name")?;

        let x = monitor
            .get("x")
            .and_then(|v| v.as_i64())
            .context("Hyprland monitor has no x position")?;

        let y = monitor
            .get("y")
            .and_then(|v| v.as_i64())
            .context("Hyprland monitor has no y position")?;

        let width = monitor
            .get("width")
            .and_then(|v| v.as_u64())
            .context("Hyprland monitor has no width")?;

        let height = monitor
            .get("height")
            .and_then(|v| v.as_u64())
            .context("Hyprland monitor has no height")?;

        let refresh_rate = monitor
            .get("refreshRate")
            .and_then(|v| v.as_f64())
            .context("Hyprland monitor has no refresh rate")?;

        let scale = monitor
            .get("scale")
            .and_then(|v| v.as_f64())
            .context("Hyprland monitor has no scale")?;

        let transform = monitor
            .get("transform")
            .and_then(|v| v.as_i64())
            .context("Hyprland monitor has no transform")?;

        states.push(MonitorState {
            name: name.to_string(),
            x: i32::try_from(x).context("Hyprland monitor x position is out of range")?,
            y: i32::try_from(y).context("Hyprland monitor y position is out of range")?,
            width: u32::try_from(width).context("Hyprland monitor width is out of range")?,
            height: u32::try_from(height).context("Hyprland monitor height is out of range")?,
            refresh_rate,
            scale,
            transform: i32::try_from(transform)
                .context("Hyprland monitor transform is out of range")?,
        });
    }

    Ok(states)
}

fn lua_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');

    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }

    escaped.push('"');
    escaped
}

/// Which config parser the running Hyprland uses; `hyprctl` speaks a
/// different dialect to each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HyprParser {
    /// The `.conf` format: `hyprctl keyword monitor <rule>`.
    Legacy,
    /// The Lua config (0.56+): `keyword` is refused, `hyprctl eval` is used.
    Lua,
}

/// The `hyprctl` invocations that reassert every saved monitor except
/// `excluded_output`, in the dialect `parser` accepts.
///
/// The Lua form must never reach a legacy-parser Hyprland, and `keyword` is
/// refused by the Lua one, so the parser actually in use decides. Returns no
/// commands when there is nothing to restore.
fn hyprland_restore_commands(
    states: &[MonitorState],
    excluded_output: &str,
    parser: HyprParser,
) -> Vec<Vec<String>> {
    let monitors: Vec<&MonitorState> = states
        .iter()
        .filter(|monitor| monitor.name != excluded_output)
        .collect();

    let refresh = |monitor: &MonitorState| {
        format!("{:.6}", monitor.refresh_rate)
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    };

    match parser {
        HyprParser::Legacy => monitors
            .into_iter()
            .map(|monitor| {
                let rule = format!(
                    "{},{}x{}@{},{}x{},{},transform,{}",
                    monitor.name,
                    monitor.width,
                    monitor.height,
                    refresh(monitor),
                    monitor.x,
                    monitor.y,
                    monitor.scale,
                    monitor.transform
                );
                vec!["keyword".to_string(), "monitor".to_string(), rule]
            })
            .collect(),
        HyprParser::Lua => {
            if monitors.is_empty() {
                return Vec::new();
            }
            let lua = monitors
                .into_iter()
                .map(|monitor| {
                    format!(
                        "hl.monitor({{ output = {}, \
                         mode = \"{}x{}@{}\", \
                         position = \"{}x{}\", \
                         scale = {}, \
                         transform = {} }})",
                        lua_string(&monitor.name),
                        monitor.width,
                        monitor.height,
                        refresh(monitor),
                        monitor.x,
                        monitor.y,
                        monitor.scale,
                        monitor.transform
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            vec![vec!["eval".to_string(), lua]]
        }
    }
}

fn restore_hyprland_monitor_state(
    states: &[MonitorState],
    excluded_output: &str,
    parser: HyprParser,
) -> Result<()> {
    for command in hyprland_restore_commands(states, excluded_output, parser) {
        let args: Vec<&str> = command.iter().map(String::as_str).collect();
        run("hyprctl", &args).context("restoring Hyprland monitor state")?;
    }
    Ok(())
}

/// A headless output, removed when dropped.
pub struct VirtualOutput {
    compositor: Compositor,
    name: String,
    /// Set when the session borrowed an output it did not create. It is put
    /// back the way it was found on drop instead of being removed.
    borrowed: Option<MonitorState>,
    /// Dialect for that restore; only meaningful on Hyprland.
    parser: HyprParser,
}

impl VirtualOutput {
    pub fn create(
        name: &str,
        width: u32,
        height: u32,
        refresh: u32,
        x: i32,
        y: i32,
    ) -> Result<Self> {
        let compositor = Compositor::detect();
        match compositor {
            // labwc cannot create an output at runtime — wlroots only builds
            // headless outputs at backend init, from `WLR_HEADLESS_OUTPUTS`.
            // So the session attaches to an output that already exists and
            // leaves it alone afterwards, rather than owning its lifetime.
            // Nothing here sizes or positions it either: `wlr-randr` could,
            // but the mode is the tablet's and the user chose this output
            // deliberately, so silently reshaping their session is worse than
            // leaving it as configured.
            Compositor::Labwc => {
                if !labwc_output_exists(name) {
                    bail!(
                        "labwc output {name:?} does not exist.\n\
                         labwc cannot create one at runtime, so start it with a \
                         headless output — `WLR_HEADLESS_OUTPUTS=1 labwc` — and \
                         pass that output's name (`wlr-randr` lists it, usually \
                         HEADLESS-1) with --output-name.\n\
                         See docs/COMPATIBILITY.md."
                    );
                }
                return Ok(Self {
                    compositor,
                    name: name.to_string(),
                    borrowed: None,
                    parser: HyprParser::Legacy,
                });
            }
            Compositor::Hyprland => {
                let saved_states = hyprland_monitor_state()
                    .context("saving Hyprland monitor state")?;

                let existing = saved_states.iter().find(|monitor| monitor.name == name);
                // An existing output by our own default name is one a crashed
                // run left behind, so it is ours to remove. Anything else the
                // user pointed us at — a monitor, a headless output they made
                // themselves — is only borrowed for the session.
                let borrowed = existing
                    .filter(|_| name != capture::VIRTUAL_OUTPUT_NAME)
                    .cloned();
                if borrowed.is_some() {
                    tracing::info!("borrowing existing output {name}; restored on exit");
                } else if existing.is_some() {
                    tracing::debug!("reusing leftover output {name}");
                } else {
                    // Hyprland accepts an explicit name here, so the result is
                    // deterministic. Without one it allocates HEADLESS-N from a
                    // counter that persists across creates and never resets —
                    // guessing the name is a latent bug.
                    run("hyprctl", &["output", "create", "headless", name])
                        .context("creating headless output")?;
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }

                // From here on the output exists, so hold it in the guard
                // before anything else can fail: an early `?` past this
                // point would otherwise leave a phantom monitor behind.
                let mut output = Self {
                    compositor,
                    name: name.to_string(),
                    borrowed,
                    parser: HyprParser::Legacy,
                };

                let spec = format!("{name},{width}x{height}@{refresh},{x}x{y},1");
                let reply = run("hyprctl", &["keyword", "monitor", &spec])
                    .with_context(|| format!("configuring output as {spec}"))?;

                // Hyprland's Lua config parser (0.56+) refuses `keyword`
                // outright — and refuses it on stdout with a zero exit
                // status, so `run` reports success and the output silently
                // keeps the compositor's defaults. Re-issue the same rule
                // through `eval`, which that parser does accept.
                let parser = if reply.contains("non-legacy parsers") {
                    let lua = format!(
                        "hl.monitor({{ output = {}, \
                         mode = \"{width}x{height}@{refresh}\", \
                         position = \"{x}x{y}\", scale = 1 }})",
                        lua_string(name)
                    );
                    run("hyprctl", &["eval", &lua])
                        .with_context(|| format!("configuring output as {lua}"))?;
                    HyprParser::Lua
                } else {
                    HyprParser::Legacy
                };
                output.parser = parser;

                restore_hyprland_monitor_state(&saved_states, name, parser)
                    .context("restoring Hyprland monitor state")?;

                std::thread::sleep(std::time::Duration::from_millis(400));
                return Ok(output);
            }
            // Sway is UNTESTED and everything else is unsupported; both cases
            // report themselves.
            other => other.ensure_supported()?,
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        Ok(Self {
            compositor,
            name: name.to_string(),
            borrowed: None,
            parser: HyprParser::Legacy,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Bind an input device (a touchscreen) to this output, so its absolute
    /// coordinates cover this output instead of the whole desktop.
    ///
    /// Only Hyprland can be told at runtime; elsewhere the user maps the
    /// device in their compositor config, and the error says so.
    pub fn bind_input_device(&self, device: &str) -> Result<()> {
        match self.compositor {
            Compositor::Hyprland => {
                let rule = match self.parser {
                    HyprParser::Legacy => vec![
                        "keyword".to_string(),
                        format!("device[{device}]:output"),
                        self.name.clone(),
                    ],
                    HyprParser::Lua => vec![
                        "eval".to_string(),
                        format!(
                            "hl.device({{ name = {}, output = {} }})",
                            lua_string(device),
                            lua_string(&self.name)
                        ),
                    ],
                };
                let args: Vec<&str> = rule.iter().map(String::as_str).collect();
                let reply = run("hyprctl", &args)?;
                // As with monitor rules, a refusal can arrive on stdout with
                // a zero exit status.
                let reply = reply.trim();
                if !reply.is_empty() && reply != "ok" {
                    bail!("hyprctl said {reply:?}");
                }
                Ok(())
            }
            other => bail!("{} cannot bind input devices at runtime", other.name()),
        }
    }

    /// Whether the requested mode and position were actually applied. False on
    /// labwc, where the output is the session's rather than ours and keeps
    /// whatever geometry it was configured with.
    pub fn applied_mode(&self) -> bool {
        self.compositor != Compositor::Labwc
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        if let Some(state) = &self.borrowed {
            match restore_hyprland_monitor_state(std::slice::from_ref(state), "", self.parser) {
                Ok(()) => tracing::debug!("restored borrowed output {}", self.name),
                Err(e) => tracing::warn!("failed to restore output {}: {e:#}", self.name),
            }
            return;
        }
        let result = match self.compositor {
            Compositor::Hyprland => run("hyprctl", &["output", "remove", &self.name]),
            Compositor::Sway => run("swaymsg", &["output", &self.name, "unplug"]),
            // Never created here, so not ours to remove.
            Compositor::Labwc => return,
            Compositor::Unsupported => return,
        };
        match result {
            Ok(_) => tracing::debug!("removed output {}", self.name),
            Err(e) => tracing::warn!("failed to remove output {}: {e}", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, x: i32, refresh_rate: f64, scale: f64) -> MonitorState {
        MonitorState {
            name: name.to_string(),
            x,
            y: 0,
            width: 1920,
            height: 1080,
            refresh_rate,
            scale,
            transform: 0,
        }
    }

    #[test]
    fn legacy_parser_restores_through_keyword_never_eval() {
        let states = [
            monitor("DP-1", 0, 59.951, 1.0),
            monitor("moreland", 1920, 60.0, 1.0),
        ];
        let commands = hyprland_restore_commands(&states, "moreland", HyprParser::Legacy);
        assert_eq!(
            commands,
            vec![vec![
                "keyword".to_string(),
                "monitor".to_string(),
                "DP-1,1920x1080@59.951,0x0,1,transform,0".to_string(),
            ]]
        );
    }

    #[test]
    fn lua_parser_restores_every_monitor_in_one_eval() {
        let states = [
            monitor("DP-1", 0, 60.0, 1.25),
            monitor("moreland", 1920, 60.0, 1.0),
            monitor("HDMI-A-1", 3840, 144.0, 1.0),
        ];
        let commands = hyprland_restore_commands(&states, "moreland", HyprParser::Lua);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0][0], "eval");
        let lua = &commands[0][1];
        assert!(lua.starts_with("hl.monitor({ output = \"DP-1\""), "{lua}");
        assert!(lua.contains("scale = 1.25"), "{lua}");
        assert!(
            lua.contains("output = \"HDMI-A-1\", mode = \"1920x1080@144\""),
            "{lua}"
        );
        assert!(!lua.contains("moreland"), "{lua}");
        assert!(!lua.ends_with(';'), "{lua}");
    }

    #[test]
    fn nothing_to_restore_issues_no_command() {
        let states = [monitor("moreland", 0, 60.0, 1.0)];
        for parser in [HyprParser::Legacy, HyprParser::Lua] {
            assert!(hyprland_restore_commands(&states, "moreland", parser).is_empty());
        }
    }

    #[test]
    fn desktop_matches_any_entry_of_the_list() {
        assert!(desktop_names("labwc:wlroots", "labwc"));
        assert!(desktop_names("labwc:wlroots", "wlroots"));
        assert!(desktop_names("Hyprland", "hyprland"));
        assert!(!desktop_names("KDE", "labwc"));
        assert!(!desktop_names("", "sway"));
        assert!(!desktop_names("swayfx", "sway"));
    }

    #[test]
    fn lua_string_escapes_quotes_and_backslashes() {
        assert_eq!(lua_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
