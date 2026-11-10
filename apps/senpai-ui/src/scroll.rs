//! How scrolling *feels*: turning wheel and touchpad input into motion.
//!
//! The rule this is built on is that the page goes exactly as far as
//! it was told to. One notch of the wheel is one fixed step, the same
//! step every time, however fast the notches come: ten of them are ten
//! steps, not ten steps and a bonus for hurry. An earlier version made
//! quick notches travel further, which let a spin of the wheel cross
//! pages and made it impossible to land on a line -- and under Wine,
//! where a touchpad arrives as a rapid run of whole notches, it turned
//! every two-finger drag into a lurch. How big the step is belongs to
//! the reader (Options -> Wheel step), not to a heuristic.
//!
//! Three things happen to input here, and no more:
//!
//! - **A wheel notch is eased.** The step is fixed; it is spread over a
//!   few frames so the eye can follow the text instead of losing its
//!   place in a jump.
//! - **A touchpad is followed directly.** Fine-grained input is applied
//!   almost as it arrives; easing it would put the page behind the
//!   fingers.
//! - **A touchpad reported in lines is geared to its speed.** Windows,
//!   and Wine with it, reports a touchpad as fractions of a wheel line
//!   rather than as a distance, so how far a "line" goes is ours to
//!   decide -- and no single answer is right. Small enough to place a
//!   line of text under slow fingers is far too small to get anywhere;
//!   large enough to get somewhere makes slow fingers twitchy. So it
//!   depends on how fast the fingers are moving: under a third of the
//!   full distance when they creep, all of it when they sweep. This is
//!   the same idea as pointer acceleration, and for the same reason.
//! - **A flick keeps going.** When fine-grained input stops while still
//!   moving fast, the page coasts on at that speed and slows down on
//!   its own. Flick again while it is coasting and the speeds add.
//!   Only fine-grained input can do this: a device that reports whole
//!   notches has no speed to carry on at, only a count.
//!
//! The coasting is done here because nothing below does it. On Linux
//! the toolkit is expected to (GTK and the browsers each implement
//! their own). On Windows proper the touchpad driver sends its own
//! decaying tail of events, which simply looks like input that slowed
//! to a stop, and so starts no second coast on top.
//!
//! Nothing in here knows about egui's scroll area, or about time except
//! as numbers it is handed, so all of it can be tested as arithmetic.

use eframe::egui;
use std::collections::VecDeque;

/// How far one wheel notch moves. The reader's choice, remembered
/// between runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WheelStep {
    /// A couple of lines of text at a reading zoom. The default: small
    /// enough to stop on the line wanted, large enough that getting
    /// down a page is not a chore. It was 30 points at first, which was
    /// exact and tiresome.
    #[default]
    Fine,
    Normal,
    /// About what a browser does.
    Fast,
}

impl WheelStep {
    pub const ALL: [Self; 3] = [Self::Fine, Self::Normal, Self::Fast];

    pub fn label(self) -> &'static str {
        match self {
            Self::Fine => "Fine",
            Self::Normal => "Normal",
            Self::Fast => "Fast",
        }
    }

    /// The name it is saved under.
    pub fn name(self) -> &'static str {
        match self {
            Self::Fine => "fine",
            Self::Normal => "normal",
            Self::Fast => "fast",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The step, in points.
    pub fn points(self) -> f32 {
        match self {
            Self::Fine => 45.0,
            Self::Normal => 80.0,
            Self::Fast => 130.0,
        }
    }
}

/// Easing time constant for a wheel notch: 63% of the way in this long,
/// visually finished in about three of them. Short, so that the page
/// has stopped by the time the finger is ready for the next notch.
const WHEEL_TAU: f32 = 0.05;
/// The same for touchpad input. Short enough to be imperceptible as
/// lag, long enough to spread one event's worth across a frame or two
/// when events and frames fall out of step.
const STREAM_TAU: f32 = 0.018;
/// How quickly a coast dies away. A flick travels its starting speed
/// times this: 3000pt/s goes 1500 points, a little over a page.
const COAST_TAU: f32 = 0.5;
/// Fine-grained input is over once nothing has arrived for this long.
/// Two frames at 60Hz: any shorter and one late event reads as the
/// fingers lifting.
const STREAM_GAP: f64 = 0.032;
/// How far back to look when measuring how fast the input was going at
/// the moment it stopped.
const VELOCITY_WINDOW: f64 = 0.09;
/// Slower than this at release and there is no coast: the fingers came
/// to rest, they did not flick.
const MIN_FLICK: f32 = 300.0;
/// Ceiling on coasting speed, so stacked flicks cannot run away.
const MAX_SPEED: f32 = 16000.0;

/// How far a touchpad's "line" goes when the fingers are moving fast.
const TOUCH_LINE_PT: f32 = 100.0;
/// The share of that it goes when they are barely moving.
const TOUCH_SLOW_GAIN: f32 = 0.3;
/// Below this speed the fingers are placing something, and get the slow
/// gain; above the second they are travelling, and get all of it. In
/// points per second at the full distance. Between the two the gain
/// rises smoothly, so there is no speed at which the page lurches.
const TOUCH_SLOW_SPEED: f32 = 120.0;
const TOUCH_FAST_SPEED: f32 = 900.0;

/// The gearing for touchpad input arriving at `speed`.
fn touch_gain(speed: f32) -> f32 {
    let t =
        ((speed.abs() - TOUCH_SLOW_SPEED) / (TOUCH_FAST_SPEED - TOUCH_SLOW_SPEED)).clamp(0.0, 1.0);
    // Smoothstep: flat at both ends, so the gain eases in and out of
    // the ramp instead of turning a corner.
    let eased = t * t * (3.0 - 2.0 * t);
    TOUCH_SLOW_GAIN + (1.0 - TOUCH_SLOW_GAIN) * eased
}

/// One piece of scroll input. Positive is further down the document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wheel {
    /// Whole clicks of a detented wheel.
    Notch(f32),
    /// Fine-grained movement in points: a touchpad that says how far
    /// the fingers went. Followed as given.
    Stream(f32),
    /// Fine-grained movement in fractions of a wheel line: a touchpad
    /// as Windows reports one. Geared by speed; see [`touch_gain`].
    Touch(f32),
}

impl Wheel {
    /// Classifies a wheel event's vertical component.
    ///
    /// The unit is not enough on its own. Windows reports everything in
    /// lines -- a touchpad as small fractions of one, a notched wheel as
    /// whole numbers -- so within that unit it is the *wholeness* of the
    /// number that tells the two apart.
    ///
    /// `line_pt` is how far one line -- one notch -- should travel.
    pub fn from_event(unit: egui::MouseWheelUnit, delta_y: f32, line_pt: f32) -> Option<Self> {
        if delta_y == 0.0 || !delta_y.is_finite() {
            return None;
        }
        // egui's sign is the content's: positive means the page moves
        // down, which is towards the *start* of the document.
        let down = -delta_y;
        Some(match unit {
            egui::MouseWheelUnit::Point => Self::Stream(down),
            egui::MouseWheelUnit::Line => {
                let whole = down.abs() >= 0.99 && (down - down.round()).abs() < 0.01;
                if whole {
                    Self::Notch(down * line_pt)
                } else {
                    // Not scaled by the wheel step: that is a setting
                    // about a wheel, and this is not one.
                    Self::Touch(down)
                }
            }
            // Rare, and coarse by definition; several notches' worth.
            egui::MouseWheelUnit::Page => Self::Notch(down * line_pt * 6.0),
        })
    }
}

/// Takes this frame's wheel input away from egui, for a [`Scroller`]
/// to have instead. Returns the frame's time, the time since the last
/// one, and the input.
///
/// Call it on the panel a scroll area is about to be shown in, before
/// showing it: the scroll area reads the same input at the end of its
/// own `show`, and whichever of the two gets there first is the one
/// that scrolls. Nothing is taken unless the pointer is over `ui`, so
/// a menu open on top keeps the wheel to itself.
///
/// Only the vertical part is taken. Sideways input is left for the
/// scroll area, which is where a page zoomed wider than the window gets
/// its panning from.
pub fn take_wheel(ui: &egui::Ui, line_pt: f32) -> (f64, f32, Vec<Wheel>) {
    let hovered = ui.rect_contains_pointer(ui.max_rect());
    ui.input_mut(|i| {
        let mut wheel = Vec::new();
        if hovered {
            for event in &i.events {
                if let egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                } = event
                {
                    // Ctrl is zoom and Shift is sideways; neither is a
                    // vertical scroll to interpret.
                    if modifiers.ctrl || modifiers.command || modifiers.shift {
                        continue;
                    }
                    wheel.extend(Wheel::from_event(*unit, delta.y, line_pt));
                }
            }
            i.smooth_scroll_delta.y = 0.0;
            i.raw_scroll_delta.y = 0.0;
        }
        // Capped: after a stall, one long frame should not be
        // integrated as if the page had been moving all that time.
        (i.time, i.stable_dt.min(0.05), wheel)
    })
}

#[derive(Default)]
pub struct Scroller {
    /// Distance asked for and not yet travelled.
    pending: f32,
    /// Time constant `pending` is being eased with, set by whatever
    /// asked last.
    tau: f32,
    /// Coasting speed, points per second.
    coast: f32,
    /// A coast interrupted by new input, kept in case that input turns
    /// out to be another flick the same way.
    carry: f32,
    /// Recent fine-grained input, one entry per frame: (time, points).
    samples: VecDeque<(f64, f32)>,
    streaming: bool,
    last_stream: f64,
    /// Recent touchpad-in-lines input before gearing, one entry per
    /// frame: (time, points at the full distance). Its speed is what
    /// the gearing goes by.
    touches: VecDeque<(f64, f32)>,
}

impl Scroller {
    /// Whether another frame is needed even if no input arrives.
    pub fn is_active(&self) -> bool {
        self.pending != 0.0 || self.coast != 0.0 || self.streaming
    }

    /// Drops everything in flight. For when the view is put somewhere
    /// by other means, or reaches an end of the document.
    pub fn stop(&mut self) {
        *self = Self::default();
    }

    /// Asks for an eased move of `points`, as a key press does.
    pub fn nudge(&mut self, points: f32) {
        self.coast = 0.0;
        self.pending += points;
        self.tau = WHEEL_TAU;
    }

    /// Advances by one frame and returns how far to scroll in it.
    ///
    /// `now` is the frame's time in seconds, `dt` the time since the
    /// last frame, `input` whatever arrived in between.
    pub fn step(&mut self, now: f64, dt: f32, input: &[Wheel]) -> f32 {
        let mut streamed = 0.0;
        let mut touched = 0.0;
        for event in input {
            match *event {
                Wheel::Notch(points) => self.notch(points),
                Wheel::Stream(points) => streamed += points,
                Wheel::Touch(lines) => touched += lines * TOUCH_LINE_PT,
            }
        }
        if touched != 0.0 {
            streamed += touched * touch_gain(self.touch_speed(now, dt, touched));
        }
        if streamed != 0.0 {
            self.stream(now, streamed);
        } else if self.streaming && now - self.last_stream > STREAM_GAP {
            self.release();
        }

        let mut moved = 0.0;
        if self.pending != 0.0 {
            let mut step = self.pending * (1.0 - (-dt / self.tau.max(0.001)).exp());
            if (self.pending - step).abs() < 0.5 {
                step = self.pending;
            }
            self.pending -= step;
            moved += step;
        }
        if self.coast != 0.0 {
            // The exact integral of an exponentially decaying speed over
            // the frame, so the distance covered does not depend on the
            // frame rate.
            let decay = (-dt / COAST_TAU).exp();
            moved += self.coast * COAST_TAU * (1.0 - decay);
            self.coast *= decay;
            if self.coast.abs() < 15.0 {
                self.coast = 0.0;
            }
        }
        moved
    }

    /// Records this frame's touchpad input and returns how fast the
    /// fingers have been going, in points per second at the full
    /// distance, averaged over the last [`VELOCITY_WINDOW`].
    ///
    /// Averaged because one frame's worth is noisy -- events and frames
    /// fall in and out of step -- and a gain that followed the noise
    /// would make the page shimmer.
    fn touch_speed(&mut self, now: f64, dt: f32, nominal: f32) -> f32 {
        self.touches.push_back((now, nominal));
        while self
            .touches
            .front()
            .is_some_and(|&(t, _)| now - t > VELOCITY_WINDOW)
        {
            self.touches.pop_front();
        }
        let first = self.touches.front().map_or(now, |&(t, _)| t);
        // The window covers the frames between its first and last
        // sample, plus the one that led up to the first.
        let span = (now - first) as f32 + dt.max(0.001);
        let total: f32 = self.touches.iter().map(|&(_, p)| p).sum();
        total / span
    }

    fn notch(&mut self, points: f32) {
        if points.signum() != self.pending.signum() && self.pending != 0.0 {
            // Reversing: what was still owed the other way is forgotten,
            // or the page would finish going down before coming back up.
            self.pending = 0.0;
        }
        self.coast = 0.0;
        self.carry = 0.0;
        self.pending += points;
        self.tau = WHEEL_TAU;
    }

    fn stream(&mut self, now: f64, points: f32) {
        if !self.streaming {
            // Fingers down. A coast in progress stops under them, as a
            // spinning thing does when touched -- but is remembered.
            self.streaming = true;
            self.carry = std::mem::take(&mut self.coast);
            self.samples.clear();
        }
        self.pending += points;
        self.tau = STREAM_TAU;
        self.last_stream = now;
        self.samples.push_back((now, points));
        while self
            .samples
            .front()
            .is_some_and(|&(t, _)| now - t > VELOCITY_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// The input has stopped: decide whether it was a flick.
    fn release(&mut self) {
        self.streaming = false;
        let speed = self.release_speed();
        let carry = std::mem::take(&mut self.carry);
        self.samples.clear();
        if speed.abs() < MIN_FLICK {
            return;
        }
        let stacked = if carry.signum() == speed.signum() {
            speed + carry
        } else {
            speed
        };
        self.coast = stacked.clamp(-MAX_SPEED, MAX_SPEED);
    }

    /// Speed of the input over its last [`VELOCITY_WINDOW`], in points
    /// per second.
    fn release_speed(&self) -> f32 {
        let (Some(&(first, _)), Some(&(last, _))) = (self.samples.front(), self.samples.back())
        else {
            return 0.0;
        };
        let frames = self.samples.len();
        if frames < 2 {
            // One frame of input has a distance but no duration.
            return 0.0;
        }
        // n samples span n-1 intervals but carry n intervals' worth of
        // movement; stretch the span to match.
        let span = (last - first) * frames as f64 / (frames - 1) as f64;
        let total: f32 = self.samples.iter().map(|&(_, p)| p).sum();
        (total as f64 / span.max(1e-3)) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;
    /// One notch, at the default step.
    const STEP: f32 = 30.0;

    /// Runs frames with no further input until the scroller settles,
    /// returning the total distance and how many frames it took.
    fn settle(s: &mut Scroller, now: &mut f64) -> (f32, usize) {
        let mut total = 0.0;
        let mut frames = 0;
        while s.is_active() {
            *now += DT as f64;
            total += s.step(*now, DT, &[]);
            frames += 1;
            assert!(frames < 2000, "never settled");
        }
        (total, frames)
    }

    /// Feeds a touchpad drag: `per_frame` points on each of `frames`
    /// consecutive frames. Returns the distance covered while dragging.
    fn drag(s: &mut Scroller, now: &mut f64, per_frame: f32, frames: usize) -> f32 {
        let mut total = 0.0;
        for _ in 0..frames {
            *now += DT as f64;
            total += s.step(*now, DT, &[Wheel::Stream(per_frame)]);
        }
        total
    }

    #[test]
    fn egui_s_sign_is_turned_into_down_the_document() {
        use egui::MouseWheelUnit::{Line, Point};
        assert_eq!(
            Wheel::from_event(Line, -1.0, STEP),
            Some(Wheel::Notch(STEP))
        );
        assert_eq!(
            Wheel::from_event(Line, 2.0, STEP),
            Some(Wheel::Notch(-2.0 * STEP))
        );
        assert_eq!(
            Wheel::from_event(Point, -7.5, STEP),
            Some(Wheel::Stream(7.5))
        );
        assert_eq!(Wheel::from_event(Point, 0.0, STEP), None);
    }

    /// Windows reports a touchpad in fractions of a line. It is kept
    /// in lines, not scaled by the wheel step.
    #[test]
    fn a_fraction_of_a_line_is_a_touchpad_not_a_notch() {
        use egui::MouseWheelUnit::Line;
        assert_eq!(
            Wheel::from_event(Line, -0.25, STEP),
            Some(Wheel::Touch(0.25))
        );
        assert_eq!(
            Wheel::from_event(Line, -1.5, 999.0),
            Some(Wheel::Touch(1.5))
        );
    }

    /// Feeds touchpad-in-lines input, `lines` on each of `frames`
    /// frames, and returns the distance covered including any coast.
    fn touch(lines: f32, frames: usize) -> f32 {
        let mut s = Scroller::default();
        let mut now = 1.0;
        let mut total = 0.0;
        for _ in 0..frames {
            now += DT as f64;
            total += s.step(now, DT, &[Wheel::Touch(lines)]);
        }
        total + settle(&mut s, &mut now).0
    }

    #[test]
    fn the_touch_gain_is_low_when_slow_full_when_fast_and_never_steps() {
        assert_eq!(touch_gain(0.0), TOUCH_SLOW_GAIN);
        assert_eq!(touch_gain(TOUCH_SLOW_SPEED), TOUCH_SLOW_GAIN);
        assert_eq!(touch_gain(TOUCH_FAST_SPEED), 1.0);
        assert_eq!(touch_gain(1.0e6), 1.0);
        // The same going up as going down.
        assert_eq!(touch_gain(-500.0), touch_gain(500.0));
        // Rising all the way, with no jump anywhere on the ramp.
        let mut last = touch_gain(0.0);
        for speed in (0..=1000).step_by(10) {
            let gain = touch_gain(speed as f32);
            assert!(gain >= last && gain - last < 0.03, "at {speed}");
            last = gain;
        }
    }

    /// Creeping fingers: a hundredth of a line a frame is 60pt/s at the
    /// full distance, well inside the slow range.
    #[test]
    fn slow_fingers_on_a_touchpad_move_the_page_a_little_at_a_time() {
        let total = touch(0.01, 60);
        let full = 0.01 * 60.0 * TOUCH_LINE_PT;
        assert!(
            (total - full * TOUCH_SLOW_GAIN).abs() < 0.5,
            "moved {total} of a possible {full}"
        );
    }

    /// Sweeping fingers: a third of a line a frame is 2000pt/s. All of
    /// the distance, and then a coast on top of it.
    #[test]
    fn fast_fingers_on_a_touchpad_go_the_full_distance_and_then_flick() {
        let total = touch(1.0 / 3.0, 9);
        let dragged = 9.0 / 3.0 * TOUCH_LINE_PT;
        assert!(
            total > dragged + 700.0,
            "went {total}, only {} past the drag",
            total - dragged
        );
    }

    /// In between there is no cliff: a little faster always goes a
    /// little further, never a lot.
    #[test]
    fn a_touchpad_gets_faster_smoothly_as_the_fingers_do() {
        let mut last = 0.0;
        for hundredths in 1..=12 {
            // Short of the flick threshold throughout, so this is the
            // gearing alone.
            let per_frame = hundredths as f32 / 100.0 * 0.35;
            let total = touch(per_frame, 30);
            assert!(total > last, "{per_frame} lines a frame went {total}");
            last = total;
        }
    }

    #[test]
    fn the_wheel_step_starts_fine_and_each_is_bigger_than_the_last() {
        assert_eq!(WheelStep::default(), WheelStep::Fine);
        for step in WheelStep::ALL {
            assert_eq!(WheelStep::from_name(step.name()), Some(step));
        }
        assert_eq!(WheelStep::from_name("warp"), None);
        assert!(WheelStep::Fine.points() < WheelStep::Normal.points());
        assert!(WheelStep::Normal.points() < WheelStep::Fast.points());
    }

    #[test]
    fn a_notch_travels_its_full_distance_but_not_all_at_once() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        let first = s.step(now, DT, &[Wheel::Notch(STEP)]);
        assert!(first > 0.0 && first < STEP * 0.5, "first frame {first}");
        let (rest, frames) = settle(&mut s, &mut now);
        assert!((first + rest - STEP).abs() < 0.01);
        // Eased, not a lurch -- and done before the next notch is due.
        assert!((4..=20).contains(&frames), "took {frames} frames");
    }

    #[test]
    fn a_notch_the_other_way_goes_the_other_way() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        let first = s.step(now, DT, &[Wheel::Notch(-STEP)]);
        let (rest, _) = settle(&mut s, &mut now);
        assert!((first + rest + STEP).abs() < 0.01);
    }

    /// The accuracy guarantee: distance is a count of notches and
    /// nothing else. Not how fast they came, not how many came before.
    #[test]
    fn every_notch_is_the_same_distance_however_fast_they_come() {
        let mut slow = Scroller::default();
        let mut now = 1.0;
        let mut slow_total = 0.0;
        for _ in 0..8 {
            slow_total += slow.step(now, DT, &[Wheel::Notch(STEP)]);
            slow_total += settle(&mut slow, &mut now).0;
            now += 0.3;
        }

        let mut fast = Scroller::default();
        let mut now = 1.0;
        let mut fast_total = 0.0;
        for _ in 0..8 {
            // A notch on every frame, and two on some.
            fast_total += fast.step(now, DT, &[Wheel::Notch(STEP)]);
            now += DT as f64;
        }
        fast_total += settle(&mut fast, &mut now).0;

        assert!((slow_total - 8.0 * STEP).abs() < 0.1, "slow {slow_total}");
        assert!((fast_total - 8.0 * STEP).abs() < 0.1, "fast {fast_total}");
    }

    /// A run of notches -- a touchpad under Wine -- ends when the
    /// notches do. There is no speed to coast on, only a count.
    #[test]
    fn notches_never_coast() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        for _ in 0..20 {
            s.step(now, DT, &[Wheel::Notch(STEP), Wheel::Notch(STEP)]);
            now += DT as f64;
        }
        assert_eq!(s.coast, 0.0);
        let (_, frames) = settle(&mut s, &mut now);
        assert!(frames <= 20, "still moving after {frames} frames");
    }

    #[test]
    fn reversing_the_wheel_does_not_finish_the_old_direction_first() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        let mut total = s.step(now, DT, &[Wheel::Notch(3.0 * STEP)]);
        now += DT as f64;
        let after_reverse = s.step(now, DT, &[Wheel::Notch(-STEP)]);
        assert!(after_reverse < 0.0, "still going down: {after_reverse}");
        total += after_reverse + settle(&mut s, &mut now).0;
        // Whatever of the first move got through, minus one notch back.
        assert!(total < 3.0 * STEP - STEP);
    }

    #[test]
    fn a_touchpad_is_followed_point_for_point() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        // 2 points a frame is 120pt/s: far too slow to be a flick.
        let mut total = drag(&mut s, &mut now, 2.0, 30);
        total += settle(&mut s, &mut now).0;
        assert!((total - 60.0).abs() < 0.01, "moved {total}");
    }

    #[test]
    fn a_flick_carries_on_after_the_fingers_lift() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        // 50 points a frame is 3000pt/s.
        let dragged = drag(&mut s, &mut now, 50.0, 6);
        let (coasted, frames) = settle(&mut s, &mut now);
        // The drag's own 300 points all arrive...
        assert!(dragged + coasted > 300.0);
        // ...and then about speed x COAST_TAU more: well over a page.
        let beyond = dragged + coasted - 300.0;
        assert!(
            (1300.0..1600.0).contains(&beyond),
            "coasted {beyond} beyond the drag"
        );
        // Over a couple of seconds, not instantly.
        assert!(frames > 60, "only {frames} frames");
    }

    #[test]
    fn a_drag_that_slows_to_a_stop_does_not_coast() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        let mut total = drag(&mut s, &mut now, 50.0, 6);
        // The fingers stay down and come to rest.
        for per_frame in [30.0, 15.0, 6.0, 2.0, 1.0, 0.5, 0.5, 0.5] {
            total += drag(&mut s, &mut now, per_frame, 1);
        }
        total += settle(&mut s, &mut now).0;
        assert!((total - 355.5).abs() < 0.01, "moved {total}");
    }

    #[test]
    fn flicking_again_while_coasting_adds_up() {
        let mut once = Scroller::default();
        let mut now = 1.0;
        let mut single = drag(&mut once, &mut now, 50.0, 6);
        single += settle(&mut once, &mut now).0;

        let mut twice = Scroller::default();
        let mut now = 1.0;
        let mut double = drag(&mut twice, &mut now, 50.0, 6);
        // Let it coast for a fifth of a second, then flick again.
        for _ in 0..12 {
            now += DT as f64;
            double += twice.step(now, DT, &[]);
        }
        double += drag(&mut twice, &mut now, 50.0, 6);
        double += settle(&mut twice, &mut now).0;
        assert!(double > 2.0 * single, "{double} vs {single}");
    }

    #[test]
    fn touching_the_pad_stops_a_coast() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        drag(&mut s, &mut now, 50.0, 6);
        for _ in 0..6 {
            now += DT as f64;
            s.step(now, DT, &[]);
        }
        assert!(s.coast > 0.0);
        // A small, slow movement: fingers resting on the pad.
        let mut after = drag(&mut s, &mut now, 1.0, 3);
        after += settle(&mut s, &mut now).0;
        assert!((after - 3.0).abs() < 0.01, "kept going: {after}");
    }

    #[test]
    fn a_flick_the_other_way_does_not_inherit_the_old_speed() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        drag(&mut s, &mut now, 50.0, 6);
        for _ in 0..6 {
            now += DT as f64;
            s.step(now, DT, &[]);
        }
        let mut back = drag(&mut s, &mut now, -50.0, 6);
        back += settle(&mut s, &mut now).0;
        // The same distance a lone upward flick covers, no less.
        assert!((-1900.0..-1600.0).contains(&back), "went {back}");
    }

    /// The distance a coast covers must not depend on the frame rate.
    #[test]
    fn a_coast_covers_the_same_ground_at_any_frame_rate() {
        let run = |dt: f32| {
            let mut s = Scroller {
                coast: 3000.0,
                ..Default::default()
            };
            let mut now = 0.0;
            let mut total = 0.0;
            while s.is_active() {
                now += dt as f64;
                total += s.step(now, dt, &[]);
            }
            total
        };
        assert!((run(1.0 / 60.0) - run(1.0 / 144.0)).abs() < 10.0);
    }

    #[test]
    fn stacked_flicks_are_capped() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        for _ in 0..40 {
            drag(&mut s, &mut now, 150.0, 5);
            for _ in 0..4 {
                now += DT as f64;
                s.step(now, DT, &[]);
            }
            assert!(s.coast.abs() <= MAX_SPEED);
        }
    }

    #[test]
    fn a_key_nudge_is_eased_like_a_notch() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        s.nudge(120.0);
        let first = s.step(now, DT, &[]);
        assert!(first > 0.0 && first < 60.0);
        let (rest, _) = settle(&mut s, &mut now);
        assert!((first + rest - 120.0).abs() < 0.01);
    }

    #[test]
    fn stop_drops_everything_in_flight() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        drag(&mut s, &mut now, 50.0, 6);
        s.nudge(500.0);
        s.stop();
        assert!(!s.is_active());
        assert_eq!(s.step(now + 1.0, DT, &[]), 0.0);
    }
}
