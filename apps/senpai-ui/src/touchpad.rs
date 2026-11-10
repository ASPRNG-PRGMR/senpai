//! Pinch-to-zoom, read straight off the touchpad.
//!
//! A pinch normally reaches a program as a gesture the platform has
//! already recognised. Under Wine it never arrives: neither of Wine's
//! display drivers handles pinch gestures, so nothing is passed on to
//! any Windows program. On native Linux the windowing library in use
//! does not report them either. Asking more politely does not help;
//! there is no event to ask for.
//!
//! What *is* there, on Linux, is the touchpad itself. The kernel
//! publishes where each finger is as an event device under
//! `/dev/input`, and a Wine process sees that same file as
//! `Z:\dev\input\...`. So this module reads the two fingers' positions
//! and works the pinch out from them: the fingers moving apart or
//! together is a zoom, and by how much is the ratio of how far apart
//! they are now to how far apart they were.
//!
//! Three limits, stated plainly:
//!
//! - **It needs permission.** The device is readable by root and the
//!   `input` group. An account in that group gets the pinch; any other
//!   gets nothing, silently, and the reader is exactly as it was. No
//!   permission is asked for and none is changed.
//! - **It reads one thing.** Only devices the kernel describes as
//!   touchpads are opened -- never a keyboard -- and of what they send
//!   only finger positions are looked at.
//! - **It is Linux's device format.** On Windows proper none of these
//!   files exist and nothing is opened. There the touchpad driver turns
//!   a pinch into Ctrl + wheel, which the reader already takes.
//!
//! The reading is a blocking loop on its own thread, as the file dialog
//! is, and costs nothing while no finger is on the pad. Telling a pinch
//! from a two-finger scroll is [`PinchDetector`], which is arithmetic
//! and tested as such.

use eframe::egui;
use std::fs::File;
use std::io::Read;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// Where the kernel's input files are. A Wine process sees the Linux
/// filesystem as drive Z.
#[cfg(windows)]
const ROOT: &str = r"Z:\";
#[cfg(not(windows))]
const ROOT: &str = "/";

/// A change in finger spread smaller than this, as a share of the
/// spread, is not yet a pinch. Two fingers resting on a pad are never
/// quite still.
const PINCH_START: f32 = 0.06;
/// The same for the fingers' midpoint moving, which is a scroll.
const SCROLL_START: f32 = 0.10;
/// How much more the spread must have changed than the midpoint has
/// moved (or the reverse) for the gesture to be called one or the
/// other. Below this it is still ambiguous and nothing is decided.
const DOMINANCE: f32 = 1.4;
/// Most fingers tracked. More than a hand has is not a pinch.
const SLOTS: usize = 10;

/// What a two-finger gesture has turned out to be.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gesture {
    /// Not enough movement yet to say.
    Undecided,
    /// The fingers are moving together: the platform's scroll. Left
    /// alone until they lift.
    Scrolling,
    Pinching,
}

/// Turns successive finger positions into zoom factors.
///
/// Decides once per touch, and sticks to it. A gesture that starts as a
/// scroll stays a scroll however the fingers drift, and one that starts
/// as a pinch stays a pinch -- otherwise a scroll with slightly
/// spreading fingers would zoom the page as it went.
#[derive(Debug)]
pub struct PinchDetector {
    gesture: Gesture,
    /// Spread and midpoint when the second finger came down.
    start: Option<(f32, [f32; 2])>,
    /// Spread at the last frame, while pinching.
    last_spread: f32,
}

impl Default for PinchDetector {
    fn default() -> Self {
        Self {
            gesture: Gesture::Undecided,
            start: None,
            last_spread: 0.0,
        }
    }
}

impl PinchDetector {
    /// Takes where the fingers are now and returns how much to zoom by,
    /// if this frame is part of a pinch. Above 1 is fingers spreading.
    pub fn frame(&mut self, fingers: &[[f32; 2]]) -> Option<f32> {
        let [a, b] = fingers else {
            // Not two fingers: whatever was going on is over.
            *self = Self::default();
            return None;
        };
        let spread = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
        let middle = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
        if spread < 1.0 {
            return None;
        }
        let Some((start_spread, start_middle)) = self.start else {
            self.start = Some((spread, middle));
            return None;
        };

        match self.gesture {
            Gesture::Scrolling => None,
            Gesture::Pinching => {
                let factor = spread / self.last_spread;
                self.last_spread = spread;
                (factor.is_finite() && factor > 0.0).then_some(factor)
            }
            Gesture::Undecided => {
                let spread_change = (spread - start_spread).abs();
                let travel = ((middle[0] - start_middle[0]).powi(2)
                    + (middle[1] - start_middle[1]).powi(2))
                .sqrt();
                if spread_change > PINCH_START * start_spread && spread_change > DOMINANCE * travel
                {
                    self.gesture = Gesture::Pinching;
                    self.last_spread = spread;
                    // The movement that proved it a pinch is part of
                    // the pinch; dropping it would make every one start
                    // with a small dead zone.
                    Some(spread / start_spread)
                } else if travel > SCROLL_START * start_spread && travel > DOMINANCE * spread_change
                {
                    self.gesture = Gesture::Scrolling;
                    None
                } else {
                    None
                }
            }
        }
    }
}

/// The event files of every touchpad in the kernel's device list
/// (`/proc/bus/input/devices`), e.g. `["event10"]`.
///
/// A touchpad is recognised by what it says it is, not by its name: it
/// reports several fingers' positions and marks itself as a pointer --
/// something that moves a cursor -- rather than as a direct device,
/// which is a touchscreen. Names vary by manufacturer; that does not.
pub fn find_touchpads(devices: &str) -> Vec<String> {
    const ABS_MT_POSITION_X: u32 = 0x35;
    const PROP_POINTER: u32 = 0;
    const PROP_DIRECT: u32 = 1;

    devices
        .split("\n\n")
        .filter_map(|block| {
            let field = |prefix: &str| {
                block
                    .lines()
                    .find_map(|line| line.trim().strip_prefix(prefix))
            };
            let abs = field("B: ABS=")?;
            let prop = field("B: PROP=")?;
            if !bit(abs, ABS_MT_POSITION_X) || !bit(prop, PROP_POINTER) || bit(prop, PROP_DIRECT) {
                return None;
            }
            field("H: Handlers=")?
                .split_whitespace()
                .find(|h| h.starts_with("event"))
                .map(str::to_owned)
        })
        .collect()
}

/// Whether bit `n` is set in one of the kernel's bitmaps, which it
/// prints as hexadecimal words, most significant first.
fn bit(bitmap: &str, n: u32) -> bool {
    let words: Vec<&str> = bitmap.split_whitespace().collect();
    // Each word is one machine word. Their width is whatever the widest
    // one printed needs, since leading zeros are dropped.
    let width = if words.iter().any(|w| w.len() > 8) {
        64
    } else {
        32
    };
    let from_end = (n / width) as usize;
    words
        .len()
        .checked_sub(1 + from_end)
        .and_then(|i| u64::from_str_radix(words[i], 16).ok())
        .is_some_and(|word| word >> (n % width) & 1 == 1)
}

/// Starts watching every readable touchpad, and returns where zoom
/// factors will arrive. `None` if there is none to watch: no touchpad,
/// no permission, or not a system that has these files at all.
pub fn watch(ctx: &egui::Context) -> Option<Receiver<f32>> {
    let devices = std::fs::read_to_string(format!("{ROOT}proc/bus/input/devices")).ok()?;
    let (tx, rx) = mpsc::channel();
    let mut watching = false;
    for event in find_touchpads(&devices) {
        let Ok(file) = File::open(format!("{ROOT}dev/input/{event}")) else {
            continue;
        };
        watching = true;
        let tx = tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || read_touchpad(file, &tx, &ctx));
    }
    watching.then_some(rx)
}

/// Reads one touchpad until it goes away or nobody is listening.
fn read_touchpad(mut file: File, tx: &Sender<f32>, ctx: &egui::Context) {
    const EV_SYN: u16 = 0;
    const EV_ABS: u16 = 3;
    const ABS_MT_SLOT: u16 = 0x2f;
    const ABS_MT_POSITION_X: u16 = 0x35;
    const ABS_MT_POSITION_Y: u16 = 0x36;
    const ABS_MT_TRACKING_ID: u16 = 0x39;

    // Each finger has a slot. A slot is in use while it has a tracking
    // id, and keeps its last position between reports: the kernel only
    // sends what changed.
    let mut down = [false; SLOTS];
    let mut at = [[0.0_f32; 2]; SLOTS];
    let mut slot = 0usize;
    let mut detector = PinchDetector::default();
    // One `struct input_event` on 64-bit Linux: a 16-byte timestamp,
    // then type, code and value.
    let mut event = [0u8; 24];

    while file.read_exact(&mut event).is_ok() {
        let kind = u16::from_ne_bytes([event[16], event[17]]);
        let code = u16::from_ne_bytes([event[18], event[19]]);
        let value = i32::from_ne_bytes([event[20], event[21], event[22], event[23]]);
        match (kind, code) {
            (EV_ABS, ABS_MT_SLOT) => slot = (value.max(0) as usize).min(SLOTS - 1),
            (EV_ABS, ABS_MT_TRACKING_ID) => down[slot] = value >= 0,
            (EV_ABS, ABS_MT_POSITION_X) => at[slot][0] = value as f32,
            (EV_ABS, ABS_MT_POSITION_Y) => at[slot][1] = value as f32,
            // End of one report: every finger's position is now current.
            (EV_SYN, 0) => {
                let fingers: Vec<[f32; 2]> =
                    (0..SLOTS).filter(|&s| down[s]).map(|s| at[s]).collect();
                if let Some(factor) = detector.frame(&fingers) {
                    if tx.send(factor).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two fingers side by side, `spread` apart, centred on `middle`.
    fn two(spread: f32, middle: [f32; 2]) -> [[f32; 2]; 2] {
        [
            [middle[0] - spread / 2.0, middle[1]],
            [middle[0] + spread / 2.0, middle[1]],
        ]
    }

    /// Feeds a run of frames and returns the product of every factor.
    fn total(detector: &mut PinchDetector, frames: &[[[f32; 2]; 2]]) -> f32 {
        frames.iter().filter_map(|f| detector.frame(f)).product()
    }

    const MID: [f32; 2] = [600.0, 400.0];

    #[test]
    fn spreading_the_fingers_zooms_in_by_how_far_they_spread() {
        let mut d = PinchDetector::default();
        let frames: Vec<_> = (0..=20)
            .map(|i| two(300.0 + 15.0 * i as f32, MID))
            .collect();
        // 300 apart to 600 apart: twice as far, twice as big. All of
        // it, including the part that went by before it was known to
        // be a pinch.
        assert!((total(&mut d, &frames) - 2.0).abs() < 1e-4);
    }

    #[test]
    fn bringing_them_together_zooms_out() {
        let mut d = PinchDetector::default();
        let frames: Vec<_> = (0..=20)
            .map(|i| two(600.0 - 15.0 * i as f32, MID))
            .collect();
        assert!((total(&mut d, &frames) - 0.5).abs() < 1e-4);
    }

    /// The one that matters most: a scroll must never zoom.
    #[test]
    fn two_fingers_moving_together_are_a_scroll_and_do_nothing() {
        let mut d = PinchDetector::default();
        let frames: Vec<_> = (0..=40)
            .map(|i| two(300.0, [MID[0], MID[1] + 12.0 * i as f32]))
            .collect();
        assert_eq!(total(&mut d, &frames), 1.0);
        assert_eq!(d.gesture, Gesture::Scrolling);
    }

    /// Fingers are not rigid. A scroll whose spread drifts by a fifth
    /// on the way is still a scroll, because it was decided early.
    #[test]
    fn a_scroll_stays_a_scroll_when_the_fingers_drift_apart() {
        let mut d = PinchDetector::default();
        let frames: Vec<_> = (0..=40)
            .map(|i| two(300.0 + 1.5 * i as f32, [MID[0], MID[1] + 12.0 * i as f32]))
            .collect();
        assert_eq!(total(&mut d, &frames), 1.0);
    }

    /// And the reverse: a pinch whose midpoint wanders stays a pinch.
    #[test]
    fn a_pinch_stays_a_pinch_when_the_hand_drifts() {
        let mut d = PinchDetector::default();
        let frames: Vec<_> = (0..=20)
            .map(|i| two(300.0 + 15.0 * i as f32, [MID[0] + 3.0 * i as f32, MID[1]]))
            .collect();
        assert!((total(&mut d, &frames) - 2.0).abs() < 1e-4);
    }

    #[test]
    fn fingers_resting_on_the_pad_do_nothing() {
        let mut d = PinchDetector::default();
        let frames: Vec<_> = (0..60)
            .map(|i| two(300.0 + (i % 3) as f32, [MID[0] + (i % 2) as f32, MID[1]]))
            .collect();
        assert_eq!(total(&mut d, &frames), 1.0);
        assert_eq!(d.gesture, Gesture::Undecided);
    }

    #[test]
    fn one_finger_or_three_is_not_a_pinch() {
        let mut d = PinchDetector::default();
        for i in 0..30 {
            assert_eq!(d.frame(&[[100.0 + 10.0 * i as f32, 100.0]]), None);
        }
        for i in 0..30 {
            let s = 10.0 * i as f32;
            assert_eq!(
                d.frame(&[[100.0 - s, 100.0], [400.0 + s, 100.0], [250.0, 300.0 + s]]),
                None
            );
        }
    }

    /// Lifting the fingers ends the gesture, whatever it was. The next
    /// touch is judged from scratch -- a scroll does not doom the pinch
    /// that follows it.
    #[test]
    fn each_touch_is_decided_afresh() {
        let mut d = PinchDetector::default();
        let scroll: Vec<_> = (0..=20)
            .map(|i| two(300.0, [MID[0], MID[1] + 12.0 * i as f32]))
            .collect();
        assert_eq!(total(&mut d, &scroll), 1.0);
        assert_eq!(d.frame(&[]), None);
        let pinch: Vec<_> = (0..=20)
            .map(|i| two(300.0 + 15.0 * i as f32, MID))
            .collect();
        assert!((total(&mut d, &pinch) - 2.0).abs() < 1e-4);
    }

    /// A new pinch starts from where the fingers land, not from where
    /// the last one left off.
    #[test]
    fn a_second_pinch_does_not_inherit_the_first_one_s_spread() {
        let mut d = PinchDetector::default();
        let first: Vec<_> = (0..=20)
            .map(|i| two(300.0 + 15.0 * i as f32, MID))
            .collect();
        total(&mut d, &first);
        d.frame(&[]);
        // Fingers land 200 apart -- a third of where the last pinch
        // ended -- and do not move.
        let still: Vec<_> = (0..10).map(|_| two(200.0, MID)).collect();
        assert_eq!(total(&mut d, &still), 1.0);
    }

    /// The device list as this machine prints it, trimmed: a keyboard,
    /// a mouse, the touchpad, and a touchscreen that must not be taken
    /// for one.
    const DEVICES: &str = "\
I: Bus=0011 Vendor=0001 Product=0001 Version=ab83
N: Name=\"AT Translated Set 2 keyboard\"
H: Handlers=sysrq kbd leds event3 
B: PROP=0
B: EV=120013
B: KEY=1100f02902000 8380307cf910f001 feffffdfffefffff fffffffffffffffe
B: MSC=10

I: Bus=0003 Vendor=046d Product=c077 Version=0111
N: Name=\"Logitech USB Optical Mouse\"
H: Handlers=mouse0 event5 
B: PROP=0
B: EV=17
B: REL=903

I: Bus=0018 Vendor=06cb Product=cfc5 Version=0100
N: Name=\"SYNA3501:00 06CB:CFC5 Touchpad\"
H: Handlers=mouse2 event10 
B: PROP=5
B: EV=1b
B: KEY=e520 10000 0 0 0 0
B: ABS=2e0800000000003
B: MSC=20

I: Bus=0018 Vendor=04f3 Product=2a1c Version=0100
N: Name=\"ELAN Touchscreen\"
H: Handlers=mouse3 event12 
B: PROP=2
B: EV=1b
B: ABS=2e0800000000003
";

    #[test]
    fn only_the_touchpad_is_picked_out_of_the_device_list() {
        assert_eq!(find_touchpads(DEVICES), vec!["event10".to_owned()]);
        assert!(find_touchpads("").is_empty());
        assert!(find_touchpads("not a device list at all").is_empty());
    }

    #[test]
    fn kernel_bitmaps_are_read_from_the_right_hand_end() {
        // Bit 0 and bit 2: a pointer that is also a button pad.
        assert!(bit("5", 0) && !bit("5", 1) && bit("5", 2));
        // Bit 0x35 of a single 64-bit word.
        assert!(bit("2e0800000000003", 0x35));
        assert!(!bit("2e0800000000003", 0x34));
        // The same bitmap as a 32-bit kernel prints it: two words.
        assert!(bit("2e08000 00000003", 0x35));
        // A bit beyond what was printed is not set.
        assert!(!bit("5", 200));
    }
}
