// SPDX-License-Identifier: Apache-2.0

//! Tablet touches, replayed on the host as a uinput multitouch screen.
//!
//! The device reports each contact in coordinates normalised across the
//! streamed image (`protocol::TOUCH_EXTENT`). The virtual device declares that
//! same range, so the compositor — once the device is bound to the virtual
//! output — maps it onto the output without this module knowing its size.
//!
//! Speaks multitouch protocol B: each contact owns a slot, and a tracking id of
//! -1 lifts it.

use anyhow::{bail, Context, Result};
use protocol::{TouchAction, TouchEvent, MAX_TOUCH_SLOTS, TOUCH_EXTENT};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;

/// Name the compositor sees; also what output binding refers to.
pub const DEVICE_NAME: &str = "moreland-touch";

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const BTN_TOUCH: u16 = 0x14a;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const INPUT_PROP_DIRECT: i32 = 0x01;
const BUS_VIRTUAL: u16 = 0x06;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

// _IOW / _IO for the 'U' (uinput) ioctl group. The direction bits are the same
// on every architecture the daemon targets (x86-64, aarch64).
const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | ((b'U' as u64) << 8) | nr
}
const fn iow<T>(nr: u64) -> u64 {
    ioc(1, nr, std::mem::size_of::<T>() as u64)
}
const UI_DEV_CREATE: u64 = ioc(0, 1, 0);
const UI_DEV_DESTROY: u64 = ioc(0, 2, 0);
const UI_DEV_SETUP: u64 = iow::<UinputSetup>(3);
const UI_ABS_SETUP: u64 = iow::<UinputAbsSetup>(4);
const UI_SET_EVBIT: u64 = iow::<i32>(100);
const UI_SET_KEYBIT: u64 = iow::<i32>(101);
const UI_SET_ABSBIT: u64 = iow::<i32>(103);
const UI_SET_PROPBIT: u64 = iow::<i32>(110);

/// An ioctl argument: an integer, or a pointer to a live `repr(C)` struct of
/// the size the request encodes.
trait IoctlArg {
    fn raw(&self) -> libc::c_ulong;
}
impl IoctlArg for i32 {
    fn raw(&self) -> libc::c_ulong {
        *self as libc::c_ulong
    }
}
impl IoctlArg for &UinputSetup {
    fn raw(&self) -> libc::c_ulong {
        *self as *const UinputSetup as libc::c_ulong
    }
}
impl IoctlArg for &UinputAbsSetup {
    fn raw(&self) -> libc::c_ulong {
        *self as *const UinputAbsSetup as libc::c_ulong
    }
}

fn ioctl(file: &File, request: u64, arg: impl IoctlArg) -> std::io::Result<()> {
    // SAFETY: the fd stays open for the lifetime of `file`, and `IoctlArg` is
    // only implemented for integers and for the structs these requests take.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request as _, arg.raw()) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Most events one `Contacts::apply` emits: slot, tracking id, two MT
/// positions, two legacy positions, BTN_TOUCH and SYN_REPORT.
const MAX_FRAME_EVENTS: usize = 8;

/// `(type, code, value)` triples, ending in a SYN_REPORT. Fixed capacity, so
/// replaying a touch allocates nothing on the thread that also times acks.
#[derive(Default)]
struct Frame {
    events: [(u16, u16, i32); MAX_FRAME_EVENTS],
    len: usize,
}

impl Frame {
    fn push(&mut self, event: (u16, u16, i32)) {
        self.events[self.len] = event;
        self.len += 1;
    }

    fn events(&self) -> &[(u16, u16, i32)] {
        &self.events[..self.len]
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The protocol-B bookkeeping, separate from the file so it can be tested.
#[derive(Default)]
struct Contacts {
    active: [bool; MAX_TOUCH_SLOTS as usize],
    /// When each live contact went down, to find the oldest.
    started: [u64; MAX_TOUCH_SLOTS as usize],
    position: [(u16, u16); MAX_TOUCH_SLOTS as usize],
    downs: u64,
    current_slot: Option<u8>,
    next_tracking_id: i32,
    /// What ABS_X/ABS_Y last said.
    legacy: Option<(u16, u16)>,
}

impl Contacts {
    fn count(&self) -> usize {
        self.active.iter().filter(|a| **a).count()
    }

    /// The contact the single-touch axes follow: the oldest one still down,
    /// as the kernel's own pointer emulation picks it.
    fn primary(&self) -> Option<usize> {
        (0..self.active.len())
            .filter(|&slot| self.active[slot])
            .min_by_key(|&slot| self.started[slot])
    }

    fn select(&mut self, slot: u8, frame: &mut Frame) {
        if self.current_slot != Some(slot) {
            frame.push((EV_ABS, ABS_MT_SLOT, i32::from(slot)));
            self.current_slot = Some(slot);
        }
    }

    fn apply(&mut self, event: TouchEvent) -> Frame {
        let mut frame = Frame::default();
        let slot = usize::from(event.slot);
        if slot >= self.active.len() {
            return frame;
        }
        let was_touching = self.count() > 0;
        let position = |contacts: &mut Self, frame: &mut Frame| {
            contacts.position[slot] = (event.x, event.y);
            frame.push((EV_ABS, ABS_MT_POSITION_X, i32::from(event.x)));
            frame.push((EV_ABS, ABS_MT_POSITION_Y, i32::from(event.y)));
        };

        match event.action {
            // A repeated down for a live slot is just a position update.
            TouchAction::Down if !self.active[slot] => {
                self.select(event.slot, &mut frame);
                self.active[slot] = true;
                self.started[slot] = self.downs;
                self.downs += 1;
                frame.push((EV_ABS, ABS_MT_TRACKING_ID, self.next_tracking_id));
                self.next_tracking_id = (self.next_tracking_id + 1) & 0xffff;
                position(self, &mut frame);
            }
            TouchAction::Down | TouchAction::Move => {
                if !self.active[slot] {
                    return frame;
                }
                self.select(event.slot, &mut frame);
                position(self, &mut frame);
            }
            TouchAction::Up => {
                if !self.active[slot] {
                    return frame;
                }
                self.select(event.slot, &mut frame);
                self.active[slot] = false;
                frame.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
            }
        }

        // Only the primary contact drives the legacy axes; when it lifts,
        // they move once to the contact that takes over.
        if let Some(primary) = self.primary() {
            let (x, y) = self.position[primary];
            if self.legacy != Some((x, y)) {
                frame.push((EV_ABS, ABS_X, i32::from(x)));
                frame.push((EV_ABS, ABS_Y, i32::from(y)));
                self.legacy = Some((x, y));
            }
        }

        let touching = self.count() > 0;
        if touching != was_touching {
            frame.push((EV_KEY, BTN_TOUCH, i32::from(touching)));
        }
        frame.push((EV_SYN, SYN_REPORT, 0));
        frame
    }

    /// Lift everything still down, as a frame per contact.
    fn release_all(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        for slot in 0..MAX_TOUCH_SLOTS {
            if self.active[usize::from(slot)] {
                frames.push(self.apply(TouchEvent {
                    action: TouchAction::Up,
                    slot,
                    x: 0,
                    y: 0,
                }));
            }
        }
        frames
    }
}

/// A uinput touchscreen, destroyed when dropped.
pub struct TouchInjector {
    file: File,
    contacts: Contacts,
}

impl TouchInjector {
    pub fn create() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context(
                "opening /dev/uinput — the user needs write access to it, \
                 see the Touch section of the README",
            )?;

        let set = |request: u64, value: i32| {
            ioctl(&file, request, value).with_context(|| format!("uinput ioctl {request:#x}"))
        };
        set(UI_SET_EVBIT, i32::from(EV_SYN))?;
        set(UI_SET_EVBIT, i32::from(EV_KEY))?;
        set(UI_SET_EVBIT, i32::from(EV_ABS))?;
        set(UI_SET_KEYBIT, i32::from(BTN_TOUCH))?;
        set(UI_SET_PROPBIT, INPUT_PROP_DIRECT)?;
        for code in [
            ABS_X,
            ABS_Y,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
            ABS_MT_SLOT,
            ABS_MT_TRACKING_ID,
        ] {
            set(UI_SET_ABSBIT, i32::from(code))?;
        }

        let abs = |code: u16, maximum: i32| -> Result<()> {
            let setup = UinputAbsSetup {
                code,
                absinfo: InputAbsinfo {
                    value: 0,
                    minimum: 0,
                    maximum,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            };
            ioctl(&file, UI_ABS_SETUP, &setup).context("uinput ABS_SETUP")
        };
        let extent = i32::from(TOUCH_EXTENT);
        for code in [ABS_X, ABS_Y, ABS_MT_POSITION_X, ABS_MT_POSITION_Y] {
            abs(code, extent)?;
        }
        abs(ABS_MT_SLOT, i32::from(MAX_TOUCH_SLOTS) - 1)?;
        abs(ABS_MT_TRACKING_ID, 0xffff)?;

        let mut name = [0u8; 80];
        name[..DEVICE_NAME.len()].copy_from_slice(DEVICE_NAME.as_bytes());
        let dev = UinputSetup {
            id: InputId {
                bustype: BUS_VIRTUAL,
                vendor: 0x1d6b,
                product: 0x4d52,
                version: 1,
            },
            name,
            ff_effects_max: 0,
        };
        ioctl(&file, UI_DEV_SETUP, &dev).context("uinput DEV_SETUP")?;
        ioctl(&file, UI_DEV_CREATE, 0).context("uinput DEV_CREATE")?;

        Ok(Self {
            file,
            contacts: Contacts::default(),
        })
    }

    pub fn inject(&mut self, event: TouchEvent) -> Result<()> {
        let frame = self.contacts.apply(event);
        self.write(&frame)
    }

    /// Lift every contact still down, so a dropped connection cannot leave a
    /// finger stuck on the host.
    pub fn release_all(&mut self) {
        for frame in self.contacts.release_all() {
            let _ = self.write(&frame);
        }
    }

    fn write(&mut self, frame: &Frame) -> Result<()> {
        const SIZE: usize = std::mem::size_of::<InputEvent>();
        if frame.is_empty() {
            return Ok(());
        }
        let mut buf = [0u8; MAX_FRAME_EVENTS * SIZE];
        for (i, &(kind, code, value)) in frame.events().iter().enumerate() {
            let event = InputEvent {
                time: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
                kind,
                code,
                value,
            };
            // SAFETY: InputEvent is repr(C) plain data, and the kernel reads
            // exactly this layout from a uinput write.
            buf[i * SIZE..(i + 1) * SIZE].copy_from_slice(unsafe {
                std::slice::from_raw_parts((&event as *const InputEvent).cast::<u8>(), SIZE)
            });
        }
        let bytes = &buf[..frame.len * SIZE];
        // One write per frame, so the kernel sees the events together.
        match self.file.write(bytes) {
            Ok(n) if n == bytes.len() => Ok(()),
            Ok(n) => bail!("short uinput write: {n} of {} bytes", bytes.len()),
            Err(e) => Err(e).context("writing to uinput"),
        }
    }
}

impl Drop for TouchInjector {
    fn drop(&mut self) {
        self.release_all();
        let _ = ioctl(&self.file, UI_DEV_DESTROY, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(action: TouchAction, slot: u8, x: u16, y: u16) -> TouchEvent {
        TouchEvent { action, slot, x, y }
    }

    fn has(frame: &Frame, kind: u16, code: u16, value: i32) -> bool {
        frame.events().contains(&(kind, code, value))
    }

    #[test]
    fn down_allocates_a_tracking_id_and_raises_btn_touch() {
        let mut c = Contacts::default();
        let frame = c.apply(ev(TouchAction::Down, 0, 100, 200));
        assert!(has(&frame, EV_ABS, ABS_MT_SLOT, 0));
        assert!(has(&frame, EV_ABS, ABS_MT_TRACKING_ID, 0));
        assert!(has(&frame, EV_ABS, ABS_MT_POSITION_X, 100));
        assert!(has(&frame, EV_ABS, ABS_MT_POSITION_Y, 200));
        assert!(has(&frame, EV_KEY, BTN_TOUCH, 1));
        assert_eq!(frame.events().last(), Some(&(EV_SYN, SYN_REPORT, 0)));
    }

    #[test]
    fn btn_touch_follows_the_first_and_last_contact_only() {
        let mut c = Contacts::default();
        c.apply(ev(TouchAction::Down, 0, 1, 1));
        let second = c.apply(ev(TouchAction::Down, 1, 2, 2));
        assert!(!second.events().iter().any(|e| e.1 == BTN_TOUCH));
        let up = c.apply(ev(TouchAction::Up, 0, 0, 0));
        assert!(!up.events().iter().any(|e| e.1 == BTN_TOUCH));
        let last = c.apply(ev(TouchAction::Up, 1, 0, 0));
        assert!(has(&last, EV_ABS, ABS_MT_TRACKING_ID, -1));
        assert!(has(&last, EV_KEY, BTN_TOUCH, 0));
    }

    #[test]
    fn slot_is_only_reselected_when_it_changes() {
        let mut c = Contacts::default();
        c.apply(ev(TouchAction::Down, 2, 1, 1));
        let moved = c.apply(ev(TouchAction::Move, 2, 5, 5));
        assert!(!moved.events().iter().any(|e| e.1 == ABS_MT_SLOT));
        c.apply(ev(TouchAction::Down, 3, 1, 1));
        let back = c.apply(ev(TouchAction::Move, 2, 6, 6));
        assert!(has(&back, EV_ABS, ABS_MT_SLOT, 2));
    }

    /// Needs write access to /dev/uinput: `cargo test -- --ignored uinput`.
    /// Holds the device open for a few seconds so it can be inspected with
    /// `hyprctl devices` or `libinput debug-events`.
    #[test]
    #[ignore]
    fn uinput_device_can_be_created_and_driven() {
        let mut injector = TouchInjector::create().expect("create uinput device");
        std::thread::sleep(std::time::Duration::from_secs(4));
        for step in 0..20u16 {
            let action = if step == 0 {
                TouchAction::Down
            } else {
                TouchAction::Move
            };
            injector
                .inject(ev(action, 0, 20000 + step * 1000, 30000))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        injector.inject(ev(TouchAction::Up, 0, 0, 0)).unwrap();
    }

    #[test]
    fn legacy_axes_follow_the_oldest_contact() {
        let mut c = Contacts::default();
        let first = c.apply(ev(TouchAction::Down, 0, 10, 10));
        assert!(has(&first, EV_ABS, ABS_X, 10));
        let second = c.apply(ev(TouchAction::Down, 1, 90, 90));
        assert!(!second
            .events()
            .iter()
            .any(|e| e.0 == EV_ABS && (e.1 == ABS_X || e.1 == ABS_Y)));
        let other = c.apply(ev(TouchAction::Move, 1, 80, 80));
        assert!(!other
            .events()
            .iter()
            .any(|e| e.0 == EV_ABS && (e.1 == ABS_X || e.1 == ABS_Y)));
        let primary = c.apply(ev(TouchAction::Move, 0, 20, 20));
        assert!(has(&primary, EV_ABS, ABS_X, 20));
        assert!(has(&primary, EV_ABS, ABS_Y, 20));
        // The first finger lifts: the axes hand over to the second, once.
        let lift = c.apply(ev(TouchAction::Up, 0, 0, 0));
        assert!(has(&lift, EV_ABS, ABS_X, 80));
        assert!(has(&lift, EV_ABS, ABS_Y, 80));
        let next = c.apply(ev(TouchAction::Move, 1, 70, 70));
        assert!(has(&next, EV_ABS, ABS_X, 70));
    }

    #[test]
    fn stray_move_and_up_emit_nothing() {
        let mut c = Contacts::default();
        assert!(c.apply(ev(TouchAction::Move, 0, 1, 1)).is_empty());
        assert!(c.apply(ev(TouchAction::Up, 0, 0, 0)).is_empty());
    }

    #[test]
    fn release_all_lifts_every_live_contact() {
        let mut c = Contacts::default();
        c.apply(ev(TouchAction::Down, 0, 1, 1));
        c.apply(ev(TouchAction::Down, 4, 1, 1));
        let frames = c.release_all();
        assert_eq!(frames.len(), 2);
        assert!(frames
            .iter()
            .all(|f| has(f, EV_ABS, ABS_MT_TRACKING_ID, -1)));
        assert_eq!(c.count(), 0);
        assert!(c.release_all().is_empty());
    }
}
