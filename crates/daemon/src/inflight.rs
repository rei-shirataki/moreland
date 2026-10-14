// SPDX-License-Identifier: Apache-2.0

//! Frames sent to the device and not yet acknowledged.
//!
//! Acks are matched to sends by pts, not by arrival order. The device only
//! acknowledges frames it actually renders: one it drops for want of an input
//! buffer, or decodes while no surface is attached, is never acked. Pairing
//! acks with sends in FIFO order lets every such frame linger in the window
//! forever, and once `max_in_flight` of them pile up the window is full while
//! acks are still arriving — every new frame is shed until `ACK_STALL` clears
//! it, which is a periodic freeze. Frames are decoded in pts order (no
//! B-frames), so an ack for pts P also settles every frame sent before it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct InFlight {
    /// `(pts in microseconds, send time)`, oldest first.
    sent: VecDeque<(u64, Instant)>,
}

impl InFlight {
    pub fn push(&mut self, pts_ns: u64, at: Instant) {
        self.sent.push_back((to_us(pts_ns), at));
    }

    /// Settle every frame up to and including `ack_pts_ns`, returning the
    /// round trip of the acknowledged frame if it was still in flight.
    ///
    /// The device echoes MediaCodec's microsecond timestamp scaled back up,
    /// so the sub-microsecond part of the original pts is lost on the way;
    /// both sides are compared in microseconds.
    pub fn ack(&mut self, ack_pts_ns: u64, at: Instant) -> Option<Duration> {
        let acked = to_us(ack_pts_ns);
        let mut round_trip = None;
        while let Some(&(pts, sent)) = self.sent.front() {
            if pts > acked {
                break;
            }
            self.sent.pop_front();
            if pts == acked {
                round_trip = Some(at.saturating_duration_since(sent));
            }
        }
        round_trip
    }

    pub fn len(&self) -> usize {
        self.sent.len()
    }

    pub fn clear(&mut self) {
        self.sent.clear();
    }
}

fn to_us(pts_ns: u64) -> u64 {
    pts_ns / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the device sends back for a frame: µs precision, scaled to ns.
    fn echoed(pts_ns: u64) -> u64 {
        pts_ns / 1000 * 1000
    }

    #[test]
    fn matches_an_ack_despite_the_microsecond_round_trip() {
        let t0 = Instant::now();
        let mut window = InFlight::default();
        // 1e9 / 90 is not a whole number of microseconds.
        window.push(11_111_111, t0);
        let rtt = window.ack(echoed(11_111_111), t0 + Duration::from_millis(20));
        assert_eq!(rtt, Some(Duration::from_millis(20)));
        assert_eq!(window.len(), 0);
    }

    #[test]
    fn frames_the_device_never_acks_do_not_stay_in_flight() {
        let t0 = Instant::now();
        let mut window = InFlight::default();
        for slot in 0..5u64 {
            window.push(slot * 11_111_111, t0 + Duration::from_millis(slot * 11));
        }
        // Slots 0..=2 were dropped or rendered without a surface; slot 3 is
        // the first ack. It must settle the three before it too, or they
        // would hold a window of 3 full forever.
        let rtt = window.ack(echoed(3 * 11_111_111), t0 + Duration::from_millis(50));
        assert_eq!(rtt, Some(Duration::from_millis(50 - 33)));
        assert_eq!(window.len(), 1);
    }

    #[test]
    fn round_trip_is_measured_against_the_acked_frame_not_the_oldest() {
        let t0 = Instant::now();
        let mut window = InFlight::default();
        window.push(0, t0);
        window.push(16_666_666, t0 + Duration::from_millis(16));
        let rtt = window.ack(echoed(16_666_666), t0 + Duration::from_millis(30));
        assert_eq!(rtt, Some(Duration::from_millis(14)));
    }

    #[test]
    fn a_late_ack_for_a_settled_frame_is_ignored() {
        let t0 = Instant::now();
        let mut window = InFlight::default();
        window.push(0, t0);
        window.push(16_666_666, t0);
        window.ack(echoed(16_666_666), t0);
        window.push(33_333_333, t0);
        assert_eq!(window.ack(echoed(0), t0), None);
        assert_eq!(window.len(), 1);
    }
}
