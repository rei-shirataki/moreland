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
//! ```
//!
//! Device → host messages are each introduced by a one-byte tag:
//!
//! ```text
//! ACK         (9 bytes)  tag 0x01, pts u64   a frame was rendered
//! TOUCH       (7 bytes)  tag 0x02, action u8, slot u8, x u16, y u16
//! ORIENTATION (5 bytes)  tag 0x03, width u16, height u16
//!                        the device's current display size, e.g. after rotation
//! ```
//!
//! Touch coordinates are normalised to `0..=TOUCH_EXTENT` across the whole
//! streamed image, so the host needs no knowledge of the tablet's panel size.
//!
//! Every multi-byte field is **big-endian**. That is deliberate: Kotlin's
//! `DataInputStream` reads big-endian natively, so the device side needs no
//! byte-swapping code at all.

use anyhow::{bail, Result};

pub const MAGIC: [u8; 4] = *b"MRLD";
pub const VERSION: u16 = 3;
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

/// Tag byte introducing each device → host message.
pub const TAG_ACK: u8 = 0x01;
pub const TAG_TOUCH: u8 = 0x02;
pub const TAG_ORIENTATION: u8 = 0x03;

/// Device → host acknowledgement, sent when a frame is handed to the display.
///
/// This exists purely to make one-way latency measurable. Host and tablet have
/// unrelated monotonic clocks, so timestamps cannot be compared directly; a
/// round trip can be. The host measures send→ack and subtracts a baseline ping
/// RTT to isolate transport plus decode.
pub const ACK_LEN: usize = 1 + 8;

/// Length of an ORIENTATION message including its tag.
pub const ORIENTATION_LEN: usize = 1 + 2 + 2;

/// Length of a TOUCH message including its tag.
pub const TOUCH_LEN: usize = 1 + 1 + 1 + 2 + 2;

/// Touch coordinates run `0..=TOUCH_EXTENT` across the streamed image.
pub const TOUCH_EXTENT: u16 = u16::MAX;

/// Simultaneous contacts carried. Touches on slots at or above this are
/// skipped.
pub const MAX_TOUCH_SLOTS: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TouchAction {
    Down = 0,
    Move = 1,
    /// Finger lifted, or the gesture was cancelled; the host treats both alike.
    Up = 2,
}

impl TouchAction {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => TouchAction::Down,
            1 => TouchAction::Move,
            2 => TouchAction::Up,
            other => bail!("unknown touch action {other}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchEvent {
    pub action: TouchAction,
    pub slot: u8,
    pub x: u16,
    pub y: u16,
}

impl TouchEvent {
    pub fn encode(&self) -> [u8; TOUCH_LEN] {
        let mut buf = [0u8; TOUCH_LEN];
        buf[0] = TAG_TOUCH;
        buf[1] = self.action as u8;
        buf[2] = self.slot;
        buf[3..5].copy_from_slice(&self.x.to_be_bytes());
        buf[5..7].copy_from_slice(&self.y.to_be_bytes());
        buf
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMessage {
    Ack { pts_ns: u64 },
    /// The device's display changed size, usually because it was rotated.
    Orientation { width: u16, height: u16 },
    Touch(TouchEvent),
    /// A well-framed message carrying values this side does not know — a
    /// touch with a newer action or a slot past `MAX_TOUCH_SLOTS`. Its length
    /// is fixed, so it is consumed and the stream carries on.
    Unsupported,
}

pub fn encode_orientation(width: u16, height: u16) -> [u8; ORIENTATION_LEN] {
    let mut buf = [0u8; ORIENTATION_LEN];
    buf[0] = TAG_ORIENTATION;
    buf[1..3].copy_from_slice(&width.to_be_bytes());
    buf[3..5].copy_from_slice(&height.to_be_bytes());
    buf
}

pub fn encode_ack(pts_ns: u64) -> [u8; ACK_LEN] {
    let mut buf = [0u8; ACK_LEN];
    buf[0] = TAG_ACK;
    buf[1..].copy_from_slice(&pts_ns.to_be_bytes());
    buf
}

/// Parse one device → host message from the front of `buf`.
///
/// Returns `Ok(None)` when `buf` holds only part of a message, so the caller
/// can read more and retry, and otherwise the message with the number of bytes
/// it occupied. An unknown tag is an error: the stream has no framing to
/// resynchronise on, so the connection cannot be trusted past it. Unknown
/// values inside a known, fixed-length message are not: they come back as
/// `DeviceMessage::Unsupported`.
pub fn parse_device_message(buf: &[u8]) -> Result<Option<(DeviceMessage, usize)>> {
    let Some(&tag) = buf.first() else {
        return Ok(None);
    };
    match tag {
        TAG_ACK => {
            if buf.len() < ACK_LEN {
                return Ok(None);
            }
            let pts_ns = u64::from_be_bytes(buf[1..ACK_LEN].try_into().unwrap());
            Ok(Some((DeviceMessage::Ack { pts_ns }, ACK_LEN)))
        }
        TAG_ORIENTATION => {
            if buf.len() < ORIENTATION_LEN {
                return Ok(None);
            }
            let message = DeviceMessage::Orientation {
                width: u16::from_be_bytes([buf[1], buf[2]]),
                height: u16::from_be_bytes([buf[3], buf[4]]),
            };
            Ok(Some((message, ORIENTATION_LEN)))
        }
        TAG_TOUCH => {
            if buf.len() < TOUCH_LEN {
                return Ok(None);
            }
            let (Ok(action), slot @ 0..MAX_TOUCH_SLOTS) = (TouchAction::from_u8(buf[1]), buf[2])
            else {
                return Ok(Some((DeviceMessage::Unsupported, TOUCH_LEN)));
            };
            let event = TouchEvent {
                action,
                slot,
                x: u16::from_be_bytes([buf[3], buf[4]]),
                y: u16::from_be_bytes([buf[5], buf[6]]),
            };
            Ok(Some((DeviceMessage::Touch(event), TOUCH_LEN)))
        }
        other => bail!("unknown device message tag 0x{other:02x}"),
    }
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
    fn ack_round_trips() {
        let buf = encode_ack(1_666_666_600);
        assert_eq!(
            parse_device_message(&buf).unwrap(),
            Some((DeviceMessage::Ack { pts_ns: 1_666_666_600 }, ACK_LEN))
        );
    }

    #[test]
    fn touch_round_trips() {
        let event = TouchEvent {
            action: TouchAction::Move,
            slot: 3,
            x: 12345,
            y: 65535,
        };
        assert_eq!(
            parse_device_message(&event.encode()).unwrap(),
            Some((DeviceMessage::Touch(event), TOUCH_LEN))
        );
    }

    #[test]
    fn partial_messages_ask_for_more() {
        let buf = encode_ack(7);
        for cut in 0..ACK_LEN {
            assert_eq!(parse_device_message(&buf[..cut]).unwrap(), None);
        }
    }

    #[test]
    fn rejects_unknown_tags() {
        assert!(parse_device_message(&[0xff]).is_err());
    }

    #[test]
    fn skips_touches_with_unknown_actions_or_slots() {
        let touch = TouchEvent {
            action: TouchAction::Down,
            slot: 0,
            x: 0,
            y: 0,
        }
        .encode();
        let mut bad_action = touch;
        bad_action[1] = 9;
        let mut bad_slot = touch;
        bad_slot[2] = MAX_TOUCH_SLOTS;
        for bad in [bad_action, bad_slot] {
            let mut wire = bad.to_vec();
            wire.extend_from_slice(&encode_ack(5));
            assert_eq!(
                parse_device_message(&wire).unwrap(),
                Some((DeviceMessage::Unsupported, TOUCH_LEN))
            );
            assert_eq!(
                parse_device_message(&wire[TOUCH_LEN..]).unwrap(),
                Some((DeviceMessage::Ack { pts_ns: 5 }, ACK_LEN))
            );
        }
    }

    #[test]
    fn messages_decode_incrementally_across_kinds() {
        let touch = TouchEvent {
            action: TouchAction::Down,
            slot: 1,
            x: 10,
            y: 20,
        };
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_ack(42));
        wire.extend_from_slice(&encode_orientation(1200, 1920));
        wire.extend_from_slice(&touch.encode());
        wire.extend_from_slice(&encode_ack(43));

        let mut rest = &wire[..];
        let mut got = Vec::new();
        while let Some((message, used)) = parse_device_message(rest).unwrap() {
            got.push(message);
            rest = &rest[used..];
        }
        assert_eq!(
            got,
            [
                DeviceMessage::Ack { pts_ns: 42 },
                DeviceMessage::Orientation { width: 1200, height: 1920 },
                DeviceMessage::Touch(touch),
                DeviceMessage::Ack { pts_ns: 43 },
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
