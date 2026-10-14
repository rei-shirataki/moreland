// SPDX-License-Identifier: Apache-2.0

//! Wire format between the host daemon and the Android app.
//!
//! One stream header at connect, then a frame header + Annex-B payload per
//! access unit. The host may send a new stream header between frames when the
//! picture size changes; the device tells it apart from a frame header by the
//! magic, which as a `u32` exceeds [`MAX_FRAME_BYTES`]:
//!
//! ```text
//! StreamHeader (16 bytes)
//!   0..4   magic  "MRLD"
//!   4..6   version          u16
//!   6..8   width            u16
//!   8..10  height           u16
//!  10..12  framerate        u16
//!     12   codec            u8
//!  13..16  reserved
//!
//! FrameHeader (16 bytes), repeated
//!   0..4   payload length   u32
//!   4..12  pts, nanoseconds u64
//!     12   flags            u8   bit 0 = keyframe
//!  13..16  reserved
//!
//! Device -> host, one of
//!   Ack          8 bytes   pts of a rendered frame, u64
//!   Orientation 16 bytes   0..8  u64::MAX (tag)
//!                          8..10 width  u16   the device's current display size
//!                         10..12 height u16
//!                         12..16 reserved
//! ```
//!
//! Every multi-byte field is **big-endian**. That is deliberate: Kotlin's
//! `DataInputStream` reads big-endian natively, so the device side needs no
//! byte-swapping code at all.

use anyhow::{bail, Result};

pub const MAGIC: [u8; 4] = *b"MRLD";
pub const VERSION: u16 = 2;
pub const STREAM_HEADER_LEN: usize = 16;
pub const FRAME_HEADER_LEN: usize = 16;

/// Host-side port for `adb forward` in the probes. The daemon lets adb pick a
/// free port per session instead.
pub const DEFAULT_PORT: u16 = 27183;
/// Abstract Unix socket the device app listens on when launched by hand.
///
/// The daemon never uses it: any app on the device can bind an abstract name
/// first, and a fixed one invites exactly that. Each session picks a fresh,
/// unguessable name instead and hands it to the app in [`SOCKET_EXTRA`].
pub const SOCKET_NAME: &str = "moreland";
/// Intent string extra naming the socket the app should listen on.
pub const SOCKET_EXTRA: &str = "com.moreland.display.SOCKET";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    H264 = 0,
    H265 = 1,
}

impl Codec {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Codec::H264,
            1 => Codec::H265,
            other => bail!("unknown codec id {other}"),
        })
    }

    /// MIME type MediaCodec expects.
    pub fn mime(self) -> &'static str {
        match self {
            Codec::H264 => "video/avc",
            Codec::H265 => "video/hevc",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StreamHeader {
    pub width: u16,
    pub height: u16,
    pub framerate: u16,
    pub codec: Codec,
}

impl StreamHeader {
    pub fn encode(&self) -> [u8; STREAM_HEADER_LEN] {
        let mut buf = [0u8; STREAM_HEADER_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..6].copy_from_slice(&VERSION.to_be_bytes());
        buf[6..8].copy_from_slice(&self.width.to_be_bytes());
        buf[8..10].copy_from_slice(&self.height.to_be_bytes());
        buf[10..12].copy_from_slice(&self.framerate.to_be_bytes());
        buf[12] = self.codec as u8;
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < STREAM_HEADER_LEN {
            bail!("stream header too short: {} bytes", buf.len());
        }
        if buf[0..4] != MAGIC {
            bail!("bad magic: {:02x?}", &buf[0..4]);
        }
        let version = u16::from_be_bytes([buf[4], buf[5]]);
        if version != VERSION {
            bail!("unsupported protocol version {version} (expected {VERSION})");
        }
        Ok(Self {
            width: u16::from_be_bytes([buf[6], buf[7]]),
            height: u16::from_be_bytes([buf[8], buf[9]]),
            framerate: u16::from_be_bytes([buf[10], buf[11]]),
            codec: Codec::from_u8(buf[12])?,
        })
    }
}

pub const FLAG_KEYFRAME: u8 = 1 << 0;

/// Largest frame payload the device accepts; it drops the connection on
/// anything bigger rather than attempt the allocation. Mirrors
/// `Protocol.MAX_FRAME_BYTES` in the Android app, so the host can refuse to
/// send one instead of having the session die on the far side.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub length: u32,
    pub pts_ns: u64,
    pub keyframe: bool,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        buf[0..4].copy_from_slice(&self.length.to_be_bytes());
        buf[4..12].copy_from_slice(&self.pts_ns.to_be_bytes());
        buf[12] = if self.keyframe { FLAG_KEYFRAME } else { 0 };
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < FRAME_HEADER_LEN {
            bail!("frame header too short: {} bytes", buf.len());
        }
        Ok(Self {
            length: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
            pts_ns: u64::from_be_bytes(buf[4..12].try_into().unwrap()),
            keyframe: buf[12] & FLAG_KEYFRAME != 0,
        })
    }
}

/// Device → host acknowledgement, sent when a frame is handed to the display.
///
/// This exists purely to make one-way latency measurable. Host and tablet have
/// unrelated monotonic clocks, so timestamps cannot be compared directly; a
/// round trip can be. The host measures send→ack and subtracts a baseline ping
/// RTT to isolate transport plus decode.
pub const ACK_LEN: usize = 8;

pub fn encode_ack(pts_ns: u64) -> [u8; ACK_LEN] {
    pts_ns.to_be_bytes()
}

pub fn decode_ack(buf: &[u8]) -> Result<u64> {
    if buf.len() < ACK_LEN {
        bail!("ack too short: {} bytes", buf.len());
    }
    Ok(u64::from_be_bytes(buf[..ACK_LEN].try_into().unwrap()))
}

/// First eight bytes of an [`DeviceMessage::Orientation`]; no real ack pts
/// reaches it.
pub const ORIENTATION_TAG: u64 = u64::MAX;
pub const ORIENTATION_LEN: usize = 16;

/// Anything the device sends to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMessage {
    Ack(u64),
    /// The device's display changed size, usually because it was rotated.
    Orientation { width: u16, height: u16 },
}

pub fn encode_orientation(width: u16, height: u16) -> [u8; ORIENTATION_LEN] {
    let mut buf = [0u8; ORIENTATION_LEN];
    buf[0..8].copy_from_slice(&ORIENTATION_TAG.to_be_bytes());
    buf[8..10].copy_from_slice(&width.to_be_bytes());
    buf[10..12].copy_from_slice(&height.to_be_bytes());
    buf
}

/// Decode one message from the front of `buf`, returning it and the bytes it
/// used, or `None` when `buf` does not yet hold a whole message.
pub fn decode_device_message(buf: &[u8]) -> Option<(DeviceMessage, usize)> {
    if buf.len() < ACK_LEN {
        return None;
    }
    let first = u64::from_be_bytes(buf[..ACK_LEN].try_into().unwrap());
    if first != ORIENTATION_TAG {
        return Some((DeviceMessage::Ack(first), ACK_LEN));
    }
    if buf.len() < ORIENTATION_LEN {
        return None;
    }
    Some((
        DeviceMessage::Orientation {
            width: u16::from_be_bytes([buf[8], buf[9]]),
            height: u16::from_be_bytes([buf[10], buf[11]]),
        },
        ORIENTATION_LEN,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_header_round_trips() {
        let header = StreamHeader {
            width: 1920,
            height: 1200,
            framerate: 60,
            codec: Codec::H264,
        };
        let decoded = StreamHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.width, 1920);
        assert_eq!(decoded.height, 1200);
        assert_eq!(decoded.framerate, 60);
        assert_eq!(decoded.codec, Codec::H264);
    }

    #[test]
    fn frame_header_round_trips() {
        let header = FrameHeader {
            length: 32357,
            pts_ns: 1_666_666_600,
            keyframe: true,
        };
        let decoded = FrameHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.length, 32357);
        assert_eq!(decoded.pts_ns, 1_666_666_600);
        assert!(decoded.keyframe);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = StreamHeader {
            width: 1,
            height: 1,
            framerate: 60,
            codec: Codec::H264,
        }
        .encode();
        buf[0] = b'X';
        assert!(StreamHeader::decode(&buf).is_err());
    }

    #[test]
    fn device_messages_decode_incrementally() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_ack(42));
        wire.extend_from_slice(&encode_orientation(1200, 1920));
        wire.extend_from_slice(&encode_ack(43));

        // A message cut short is not yet decodable.
        assert_eq!(decode_device_message(&wire[..7]), None);
        assert_eq!(decode_device_message(&wire[8..8 + 15]), None);

        let mut rest = &wire[..];
        let mut got = Vec::new();
        while let Some((message, used)) = decode_device_message(rest) {
            got.push(message);
            rest = &rest[used..];
        }
        assert_eq!(
            got,
            [
                DeviceMessage::Ack(42),
                DeviceMessage::Orientation { width: 1200, height: 1920 },
                DeviceMessage::Ack(43),
            ]
        );
        assert!(rest.is_empty());
    }

    #[test]
    fn stream_header_magic_is_not_a_plausible_frame_length() {
        let magic = u32::from_be_bytes(MAGIC);
        assert!(magic as usize > MAX_FRAME_BYTES);
    }
}
