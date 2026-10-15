// SPDX-License-Identifier: Apache-2.0

//! Framed video transport to the device over `adb forward`.

pub mod adb;
pub mod process;

use anyhow::{bail, Context, Result};
use protocol::{Codec, FrameHeader, StreamHeader};
use std::io::{IoSlice, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Sender {
    stream: TcpStream,
    frames_sent: u64,
    bytes_sent: u64,
}

impl Sender {
    /// Connect to the adb-forwarded port and send the stream header.
    pub fn connect(port: u16, header: &StreamHeader) -> Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .with_context(|| format!("connecting to forwarded port {port}"))?;

        // Nagle batches small writes waiting for an ack. On a per-frame video
        // stream that is pure added latency, and our frames are already
        // sized deliberately.
        stream
            .set_nodelay(true)
            .context("disabling Nagle on the transport socket")?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        let mut sender = Self {
            stream,
            frames_sent: 0,
            bytes_sent: 0,
        };
        let encoded = header.encode();
        sender
            .stream
            .write_all(&encoded)
            .context("sending stream header")?;
        sender.bytes_sent += encoded.len() as u64;
        Ok(sender)
    }

    /// Announce a new picture size between frames. The device reconfigures its
    /// decoder; the next frame sent must be a keyframe.
    pub fn send_header(&mut self, header: &StreamHeader) -> Result<()> {
        let encoded = header.encode();
        self.stream
            .write_all(&encoded)
            .context("sending stream header")?;
        self.bytes_sent += encoded.len() as u64;
        Ok(())
    }

    /// Send one access unit. Header and payload go out in a single vectored
    /// write so they cannot be split into separate segments.
    pub fn send_frame(&mut self, payload: &[u8], pts_ns: u64, keyframe: bool) -> Result<()> {
        if payload.len() > protocol::MAX_FRAME_BYTES {
            bail!(
                "encoded frame of {} bytes exceeds the device's {}-byte limit; \
                 lower --bitrate or the resolution",
                payload.len(),
                protocol::MAX_FRAME_BYTES
            );
        }
        let header = FrameHeader {
            // Bounded by MAX_FRAME_BYTES above, so this cannot truncate.
            length: payload.len() as u32,
            pts_ns,
            keyframe,
        }
        .encode();

        let mut slices = [IoSlice::new(&header), IoSlice::new(payload)];
        write_all_vectored(&mut self.stream, &mut slices).context("sending frame")?;

        self.frames_sent += 1;
        self.bytes_sent += (header.len() + payload.len()) as u64;
        Ok(())
    }

    /// A second handle on the same socket for reading device acknowledgements.
    ///
    /// Acks flow device→host while frames flow host→device, so the two
    /// directions can be driven from separate threads without locking.
    pub fn ack_reader(&self) -> Result<TcpStream> {
        let stream = self
            .stream
            .try_clone()
            .context("cloning transport socket for acks")?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(stream)
    }

    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }
}

/// `Write::write_all_vectored` is still unstable, so advance the slices by hand.
fn write_all_vectored(writer: &mut impl Write, slices: &mut [IoSlice<'_>]) -> std::io::Result<()> {
    let mut slices = slices;
    while !slices.is_empty() {
        let written = writer.write_vectored(slices)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "transport socket accepted no bytes",
            ));
        }
        IoSlice::advance_slices(&mut slices, written);
    }
    Ok(())
}

/// Convenience for the common case.
///
/// The wire fields are 16-bit. A value that does not fit is an error rather
/// than silently truncated, which would tell the decoder a different size
/// than the encoder produces.
pub fn stream_header(width: u32, height: u32, framerate: u32) -> Result<StreamHeader> {
    let field = |name: &str, value: u32| {
        u16::try_from(value).with_context(|| format!("stream {name} {value} exceeds {}", u16::MAX))
    };
    Ok(StreamHeader {
        width: field("width", width)?,
        height: field("height", height)?,
        framerate: field("framerate", framerate)?,
        codec: Codec::H264,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_header_rejects_values_the_wire_cannot_carry() {
        assert!(stream_header(1920, 1200, 90).is_ok());
        assert!(stream_header(70_000, 1200, 90).is_err());
        assert!(stream_header(1920, 1200, 70_000).is_err());
    }

    #[test]
    fn oversized_frames_are_refused_before_anything_is_written() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut sender = Sender::connect(port, &stream_header(16, 16, 60).unwrap()).unwrap();
        let before = sender.bytes_sent();
        let huge = vec![0u8; protocol::MAX_FRAME_BYTES + 1];
        assert!(sender.send_frame(&huge, 0, true).is_err());
        assert_eq!(sender.bytes_sent(), before);
        assert!(sender.send_frame(&[0u8; 16], 0, true).is_ok());
    }
}
