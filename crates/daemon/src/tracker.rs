// SPDX-License-Identifier: Apache-2.0

//! Device hotplug detection over the ADB server protocol.
//!
//! `host:track-devices` streams a fresh device list every time one changes,
//! which is why this project needs no udev rule. Polling `adb devices` would
//! also work but adds latency and wakeups; matching on USB product IDs would be
//! worse still, since the tablet's ID shifts with USB mode (`ff40` MTP-only vs
//! `ff48` MTP+ADB — see docs/00-system-survey.md).

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

const ADB_SERVER: &str = "127.0.0.1:5037";

/// How long a message, once begun, may take to arrive in full. The ADB server
/// writes each one at once, so this only bounds a server that died mid-write.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    pub state: String,
}

impl Device {
    /// Only `device` state can be forwarded to; `unauthorized` and `offline`
    /// look connected but reject every request.
    pub fn is_ready(&self) -> bool {
        self.state == "device"
    }
}

pub struct DeviceTracker {
    stream: TcpStream,
}

impl DeviceTracker {
    pub fn connect() -> Result<Self> {
        // Idempotent; brings the server up if this is the first client.
        let _ = Command::new(
            std::env::var("MORELAND_ADB").unwrap_or_else(|_| "adb".into()),
        )
        .arg("start-server")
        .output();

        let mut stream = TcpStream::connect(ADB_SERVER)
            .with_context(|| format!("connecting to the ADB server at {ADB_SERVER}"))?;

        send_request(&mut stream, "host:track-devices")?;
        Ok(Self { stream })
    }

    /// Wait up to `wait` for the device list to change. `None` means it did
    /// not, which is the normal case: the last list still stands.
    ///
    /// Only the wait for a message to *begin* is bounded by `wait`. A read
    /// timeout that fired partway through `read_exact` would discard the bytes
    /// already consumed and leave the stream mid-message, so the next read
    /// would parse payload as a length. Peeking does not consume, and once a
    /// message has started it is read whole.
    pub fn next_update(&mut self, wait: Duration) -> Result<Option<Vec<Device>>> {
        self.stream.set_read_timeout(Some(wait))?;
        match self.stream.peek(&mut [0u8; 1]) {
            Ok(0) => bail!("the ADB server closed the device tracker"),
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None)
            }
            Err(e) => return Err(e).context("waiting for a device list update"),
        }

        self.stream.set_read_timeout(Some(MESSAGE_TIMEOUT))?;
        let payload = read_payload(&mut self.stream)?;
        Ok(Some(parse_devices(&payload)))
    }
}

/// ADB frames requests as a 4-digit hex length followed by the payload.
fn send_request(stream: &mut TcpStream, request: &str) -> Result<()> {
    write!(stream, "{:04x}{}", request.len(), request)
        .with_context(|| format!("sending ADB request {request:?}"))?;
    stream.flush()?;

    let mut status = [0u8; 4];
    stream
        .read_exact(&mut status)
        .context("reading ADB response status")?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let reason = read_payload(stream).unwrap_or_default();
            bail!("ADB rejected {request:?}: {reason}");
        }
        other => bail!("unexpected ADB status {:?}", String::from_utf8_lossy(other)),
    }
}

fn read_payload(stream: &mut TcpStream) -> Result<String> {
    let mut len_hex = [0u8; 4];
    stream.read_exact(&mut len_hex).context("reading ADB length")?;
    let len = usize::from_str_radix(
        std::str::from_utf8(&len_hex).context("ADB length was not UTF-8")?,
        16,
    )
    .context("ADB length was not hex")?;

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).context("reading ADB payload")?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_devices(payload: &str) -> Vec<Device> {
    payload
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let serial = fields.next()?;
            let state = fields.next()?;
            Some(Device {
                serial: serial.to_string(),
                state: state.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_list() {
        let devices = parse_devices("ABCD1234\tdevice\nemulator-5554\toffline\n");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].serial, "ABCD1234");
        assert!(devices[0].is_ready());
        assert!(!devices[1].is_ready());
    }

    #[test]
    fn empty_list_is_no_devices() {
        assert!(parse_devices("").is_empty());
    }

    fn tracker_with_server() -> (DeviceTracker, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (DeviceTracker { stream: client }, server)
    }

    #[test]
    fn no_update_within_the_wait_is_not_an_error() {
        let (mut tracker, _server) = tracker_with_server();
        assert!(tracker.next_update(Duration::from_millis(20)).unwrap().is_none());
    }

    #[test]
    fn a_message_split_across_the_wait_is_read_whole() {
        let (mut tracker, mut server) = tracker_with_server();
        let payload = "ABCD1234\tdevice\n";
        let writer = std::thread::spawn(move || {
            // Length first, then the body well after the wait has elapsed.
            write!(server, "{:04x}", payload.len()).unwrap();
            std::thread::sleep(Duration::from_millis(150));
            server.write_all(payload.as_bytes()).unwrap();
            write!(server, "{:04x}{}", payload.len(), payload).unwrap();
            server
        });

        let first = tracker.next_update(Duration::from_millis(30)).unwrap();
        assert_eq!(first.unwrap()[0].serial, "ABCD1234");
        // Still aligned: the following message parses too.
        let second = tracker.next_update(Duration::from_millis(500)).unwrap();
        assert!(second.unwrap()[0].is_ready());
        drop(writer.join().unwrap());
    }

    #[test]
    fn a_closed_tracker_is_an_error() {
        let (mut tracker, server) = tracker_with_server();
        drop(server);
        assert!(tracker.next_update(Duration::from_millis(200)).is_err());
    }
}
