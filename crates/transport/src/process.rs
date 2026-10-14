// SPDX-License-Identifier: Apache-2.0

//! Running helper programs (`adb`, `hyprctl`, `wlr-randr`) with a deadline.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Run `cmd` to completion, capturing stdout and stderr, and kill it if it
/// takes longer than `timeout`.
///
/// Both pipes are drained on their own threads while the child runs. Reading
/// them only after it exits would deadlock on any command that writes more
/// than a pipe buffer (about 64 KiB): the child blocks in `write()` and never
/// exits, so it would be killed at the deadline and reported as timed out.
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {:?}", cmd.get_program()))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let start = Instant::now();
    // Most of these commands finish in a few milliseconds, so start polling
    // fast and back off rather than adding a fixed delay to every call.
    let mut poll = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{:?} timed out after {timeout:?}", cmd.get_program());
        }
        std::thread::sleep(poll);
        poll = (poll * 2).min(Duration::from_millis(20));
    };

    Ok(Output {
        status,
        stdout: join(stdout),
        stderr: join(stderr),
    })
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Option<JoinHandle<Vec<u8>>> {
    pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    })
}

fn join(handle: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        let out = run_with_timeout(
            Command::new("sh").args(["-c", "head -c 1000000 /dev/zero; echo err >&2"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 1_000_000);
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn a_hung_command_is_killed_at_the_deadline() {
        let start = Instant::now();
        let err = run_with_timeout(
            Command::new("sh").args(["-c", "exec sleep 10"]),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
