//! How scrolling *feels*: turning wheel and touchpad input into motion.
//!
//! egui's own handling is one rule for everything -- a wheel notch is 50
//! points, eased -- and nothing carries on once the input stops. In a
//! document that reads as a row of small identical steps, the same as
//! holding an arrow key, and a flick of the touchpad goes exactly as far
//! as the fingers did. A browser does three things differently, and
//! this module is those three things:
//!
//! - **A wheel notch travels further and glides.** About a hundred
//!   points, eased out over a fifth of a second, so the page moves
//!   rather than jumps. Notches in quick succession each go a little
//!   further than the last, which is what lets a spin of the wheel
//!   cross pages instead of paragraphs.
//! - **A touchpad is followed directly.** Fine-grained input is applied
//!   almost as it arrives; easing it would put the page behind the
//!   fingers.
//! - **A flick keeps going.** When fine-grained input stops while still
//!   moving fast, the page coasts on at that speed and slows down on
//!   its own. Flick again while it is coasting and the speeds add, so
//!   three flicks cover a chapter.
//!
//! The coasting is done here because nothing below does it. On Linux
//! the toolkit is expected to (GTK and the browsers each implement
//! their own); under Wine no inertia arrives from anywhere. On Windows
//! proper the touchpad driver sends its own decaying tail of events,
//! which simply looks like input that slowed to a stop, and so starts
//! no second coast on top.
//!
//! Nothing in here knows about egui's scroll area, or about time except
//! as numbers it is handed, so all of it can be tested as arithmetic.

use eframe::egui;
use std::collections::VecDeque;

/// How far one wheel notch moves, in points.
pub const LINE_PT: f32 = 100.0;
/// Easing time constant for a wheel notch: 63% of the way in this long,
/// visually finished in about three of them.
const WHEEL_TAU: f32 = 0.075;
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
/// Notches closer together than this are one spin of the wheel.
const NOTCH_CHAIN: f64 = 0.11;
/// Extra distance per notch for each notch already in the spin, and
/// where that stops growing.
const NOTCH_ACCEL: f32 = 0.25;
const NOTCH_ACCEL_MAX: f32 = 4.0;

/// One piece of scroll input. Positive is further down the document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wheel {
    /// Whole clicks of a detented wheel.
    Notch(f32),
    /// Fine-grained movement in points: a touchpad, or a wheel with no
    /// detents.
    Stream(f32),
}

impl Wheel {
    /// Classifies a wheel event's vertical component.
    ///
    /// The unit is not enough on its own. Windows reports everything in
    /// lines -- a touchpad as small fractions of one, a notched wheel as
    /// whole numbers -- so within that unit it is the *wholeness* of the
    /// number that tells the two apart.
    pub fn from_event(unit: egui::MouseWheelUnit, delta_y: f32) -> Option<Self> {
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
                    Self::Notch(down)
                } else {
                    Self::Stream(down * LINE_PT)
                }
            }
            // Rare, and coarse by definition; several notches' worth.
            egui::MouseWheelUnit::Page => Self::Notch(down * 6.0),
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
pub fn take_wheel(ui: &egui::Ui) -> (f64, f32, Vec<Wheel>) {
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
                    wheel.extend(Wheel::from_event(*unit, delta.y));
                }
            }
            i.smooth_scroll_delta = egui::Vec2::ZERO;
            i.raw_scroll_delta = egui::Vec2::ZERO;
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
    last_notch: f64,
    notch_run: u32,
    notch_dir: f32,
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
        for event in input {
            match *event {
                Wheel::Notch(lines) => self.notch(now, lines),
                Wheel::Stream(points) => streamed += points,
            }
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

    fn notch(&mut self, now: f64, lines: f32) {
        let dir = lines.signum();
        if dir == self.notch_dir && now - self.last_notch <= NOTCH_CHAIN {
            self.notch_run += 1;
        } else {
            self.notch_run = 0;
        }
        if dir != self.pending.signum() && self.pending != 0.0 {
            // Reversing: what was still owed the other way is forgotten,
            // or the page would finish going down before coming back up.
            self.pending = 0.0;
        }
        self.notch_dir = dir;
        self.last_notch = now;
        self.coast = 0.0;
        self.carry = 0.0;
        let accel = (1.0 + NOTCH_ACCEL * self.notch_run as f32).min(NOTCH_ACCEL_MAX);
        self.pending += lines * LINE_PT * accel;
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
        assert_eq!(Wheel::from_event(Line, -1.0), Some(Wheel::Notch(1.0)));
        assert_eq!(Wheel::from_event(Line, 2.0), Some(Wheel::Notch(-2.0)));
        assert_eq!(Wheel::from_event(Point, -7.5), Some(Wheel::Stream(7.5)));
        assert_eq!(Wheel::from_event(Point, 0.0), None);
    }

    /// Windows reports a touchpad in fractions of a line.
    #[test]
    fn a_fraction_of_a_line_is_fine_grained_input_not_a_notch() {
        use egui::MouseWheelUnit::Line;
        assert_eq!(
            Wheel::from_event(Line, -0.25),
            Some(Wheel::Stream(0.25 * LINE_PT))
        );
        assert_eq!(
            Wheel::from_event(Line, -1.5),
            Some(Wheel::Stream(1.5 * LINE_PT))
        );
    }

    #[test]
    fn a_notch_travels_its_full_distance_but_not_all_at_once() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        let first = s.step(now, DT, &[Wheel::Notch(1.0)]);
        assert!(first > 0.0 && first < LINE_PT * 0.5, "first frame {first}");
        let (rest, frames) = settle(&mut s, &mut now);
        assert!((first + rest - LINE_PT).abs() < 0.01);
        // A glide, not a lurch -- and not a crawl either.
        assert!((6..=30).contains(&frames), "took {frames} frames");
    }

    #[test]
    fn a_notch_the_other_way_goes_the_other_way() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        let first = s.step(now, DT, &[Wheel::Notch(-1.0)]);
        let (rest, _) = settle(&mut s, &mut now);
        assert!((first + rest + LINE_PT).abs() < 0.01);
    }

    #[test]
    fn a_spin_of_the_wheel_goes_further_than_the_same_notches_taken_slowly() {
        let mut slow = Scroller::default();
        let mut now = 1.0;
        let mut slow_total = 0.0;
        for _ in 0..8 {
            slow_total += slow.step(now, DT, &[Wheel::Notch(1.0)]);
            slow_total += settle(&mut slow, &mut now).0;
            now += 0.3;
        }
        assert!((slow_total - 8.0 * LINE_PT).abs() < 0.1);

        let mut fast = Scroller::default();
        let mut now = 1.0;
        let mut fast_total = 0.0;
        for _ in 0..8 {
            // A notch every other frame.
            fast_total += fast.step(now, DT, &[Wheel::Notch(1.0)]);
            now += DT as f64;
            fast_total += fast.step(now, DT, &[]);
            now += DT as f64;
        }
        fast_total += settle(&mut fast, &mut now).0;
        assert!(
            fast_total > 1.5 * slow_total,
            "spin {fast_total} vs slow {slow_total}"
        );
    }

    #[test]
    fn reversing_the_wheel_does_not_finish_the_old_direction_first() {
        let mut s = Scroller::default();
        let mut now = 1.0;
        let mut total = s.step(now, DT, &[Wheel::Notch(3.0)]);
        now += DT as f64;
        let after_reverse = s.step(now, DT, &[Wheel::Notch(-1.0)]);
        assert!(after_reverse < 0.0, "still going down: {after_reverse}");
        total += after_reverse + settle(&mut s, &mut now).0;
        // Whatever of the first move got through, minus one notch back.
        assert!(total < 3.0 * LINE_PT - LINE_PT);
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
