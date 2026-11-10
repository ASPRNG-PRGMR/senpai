//! The whole reader, driven frame by frame without a window.
//!
//! The unit tests beside each module check arithmetic. These check that
//! a key or a gesture, arriving the way egui delivers it, gets all the
//! way through the app to what is on screen -- the kind of fault where
//! every part is right and the wire between two of them is missing.
//!
//! A real PDF is opened and the real render worker runs; only the
//! window and the GPU are absent.

use super::*;
use continuous::{fit_scale, Fit};
use eframe::egui::{pos2, vec2, Event, Key, Modifiers, Pos2, RawInput, Rect};
use std::sync::Once;

const SCREEN: egui::Vec2 = vec2(1000.0, 750.0);
const LETTER: PageSize = PageSize {
    width_pt: 612.0,
    height_pt: 792.0,
};

/// A structurally valid PDF of `pages` blank US Letter pages.
fn letter_pdf(pages: usize) -> Vec<u8> {
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} >>",
            kids.join(" ")
        ),
    ];
    for _ in 0..pages {
        objects.push("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string());
    }
    let mut out: Vec<u8> = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// Points the settings directory somewhere disposable, once, before any
/// app is built. Without it these tests would read the developer's own
/// bindings and write to their recent-documents list.
fn scratch_dir() -> PathBuf {
    static ONCE: Once = Once::new();
    let dir = std::env::temp_dir().join(format!("senpai-app-tests-{}", std::process::id()));
    ONCE.call_once(|| {
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        std::env::set_var("APPDATA", dir.join("config"));
    });
    dir
}

struct Reader {
    ctx: egui::Context,
    app: SenpaiApp,
    time: f64,
}

impl Reader {
    /// A reader with a ten-page document open, in `mode`, after one
    /// frame so that everything has been laid out.
    fn open(name: &str, mode: ViewMode) -> Self {
        let path = scratch_dir().join(format!("{name}.pdf"));
        std::fs::write(&path, letter_pdf(10)).expect("write test pdf");
        let ctx = egui::Context::default();
        let mut app = SenpaiApp::with_context(&ctx, Some(path));
        assert_eq!(app.open_error, None);
        assert_eq!(app.page_count, 10);
        // Set directly rather than through `set_view_mode`, which saves.
        app.view_mode = mode;
        app.nav_keys = NavKeys::default();
        let mut reader = Self {
            ctx,
            app,
            time: 0.0,
        };
        // The pointer over the page, as it would be for someone using it.
        reader.frame(vec![Event::PointerMoved(pos2(500.0, 400.0))]);
        reader.frame(vec![]);
        reader
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.frame_with(events, Modifiers::NONE);
    }

    fn frame_with(&mut self, events: Vec<Event>, modifiers: Modifiers) {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
            time: Some(self.time),
            predicted_dt: 1.0 / 60.0,
            modifiers,
            events,
            ..Default::default()
        };
        let app = &mut self.app;
        let _ = self.ctx.run(input, |ctx| app.frame(ctx));
    }

    fn press(&mut self, key: Key) {
        self.frame(vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }]);
        // And a frame for what the key asked for to be drawn.
        self.frame(vec![]);
    }

    /// The zoom level pages are actually being drawn at.
    fn shown(&self) -> f64 {
        self.app.shown_scale
    }
}

fn both_modes(test: impl Fn(Reader)) {
    for (name, mode) in [
        ("paged", ViewMode::Paged),
        ("continuous", ViewMode::Continuous),
    ] {
        let thread = std::thread::current();
        let test_name = thread.name().unwrap_or("test").replace("::", "-");
        test(Reader::open(&format!("{test_name}-{name}"), mode));
    }
}

#[test]
fn a_document_opens_fitted_to_the_page() {
    both_modes(|r| {
        assert_eq!(r.app.fit, Some(Fit::Page));
        assert_eq!(r.shown(), fit_scale(Fit::Page, LETTER, SCREEN));
    });
}

#[test]
fn w_fits_the_width_and_f_fits_the_page() {
    both_modes(|mut r| {
        r.press(Key::W);
        assert_eq!(r.app.fit, Some(Fit::Width));
        assert_eq!(r.shown(), fit_scale(Fit::Width, LETTER, SCREEN));
        assert!(r.shown() > fit_scale(Fit::Page, LETTER, SCREEN) * 1.3);

        r.press(Key::F);
        assert_eq!(r.app.fit, Some(Fit::Page));
        assert_eq!(r.shown(), fit_scale(Fit::Page, LETTER, SCREEN));
    });
}

/// From a zoom level set by hand, too -- which is where F is most
/// needed, as the way back.
#[test]
fn f_and_w_work_from_a_hand_set_zoom() {
    both_modes(|mut r| {
        r.press(Key::Plus);
        r.press(Key::Plus);
        assert_eq!(r.app.fit, None);
        r.press(Key::F);
        assert_eq!(r.shown(), fit_scale(Fit::Page, LETTER, SCREEN));
        r.press(Key::Minus);
        r.press(Key::W);
        assert_eq!(r.shown(), fit_scale(Fit::Width, LETTER, SCREEN));
    });
}

#[test]
fn plus_and_minus_step_from_wherever_the_fit_had_got_to() {
    both_modes(|mut r| {
        let fitted = r.shown();
        r.press(Key::Plus);
        assert_eq!(r.app.fit, None);
        assert!((r.shown() - fitted * ZOOM_STEP).abs() < 1e-9);
        r.press(Key::Minus);
        assert!((r.shown() - fitted).abs() < 1e-9);
    });
}

/// A key the reader bound to a direction keeps that meaning.
#[test]
fn w_stays_a_direction_for_someone_who_bound_it_to_one() {
    both_modes(|mut r| {
        r.app.nav_keys.apply(Preset::Gamer);
        r.press(Key::W);
        assert_eq!(r.app.fit, Some(Fit::Page));
        // F is not one of that preset's keys, and still fits.
        r.press(Key::Plus);
        r.press(Key::F);
        assert_eq!(r.app.fit, Some(Fit::Page));
    });
}

/// F on a page that is already fitted changes nothing on screen. It
/// still has to be acknowledged, or it reads as a dead key.
#[test]
fn a_fit_key_is_acknowledged_even_when_nothing_changes() {
    both_modes(|mut r| {
        assert!(r.time >= r.app.zoom_note_until);
        r.press(Key::F);
        assert_eq!(r.app.fit, Some(Fit::Page));
        assert!(r.app.zoom_note_until > r.time);
        // And it goes away again.
        for _ in 0..120 {
            r.frame(vec![]);
        }
        assert!(r.time >= r.app.zoom_note_until);
    });
}

/// How a pinch arrives where the platform reports one as a gesture.
#[test]
fn a_pinch_zooms() {
    both_modes(|mut r| {
        let before = r.shown();
        r.frame(vec![Event::Zoom(1.25)]);
        r.frame(vec![]);
        assert_eq!(r.app.fit, None);
        assert!(
            (r.shown() - before * 1.25).abs() < 1e-6,
            "{before} became {}",
            r.shown()
        );
        r.frame(vec![Event::Zoom(0.5)]);
        r.frame(vec![]);
        assert!((r.shown() - before * 1.25 * 0.5).abs() < 1e-6);
    });
}

/// How a pinch arrives on Windows: the touchpad driver turns it into
/// Ctrl held and the wheel turned. Also simply Ctrl and the wheel.
#[test]
fn ctrl_and_the_wheel_zoom_and_do_not_scroll() {
    both_modes(|mut r| {
        let before = r.shown();
        let page_before = r.app.current_page;
        for _ in 0..3 {
            r.frame_with(
                vec![Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: vec2(0.0, 1.0),
                    modifiers: Modifiers::CTRL,
                }],
                Modifiers::CTRL,
            );
        }
        for _ in 0..30 {
            r.frame_with(vec![], Modifiers::CTRL);
        }
        assert!(r.shown() > before * 1.2, "{before} became {}", r.shown());
        assert_eq!(r.app.current_page, page_before);

        // And the other way.
        let zoomed = r.shown();
        for _ in 0..3 {
            r.frame_with(
                vec![Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: vec2(0.0, -1.0),
                    modifiers: Modifiers::CTRL,
                }],
                Modifiers::CTRL,
            );
        }
        for _ in 0..30 {
            r.frame_with(vec![], Modifiers::CTRL);
        }
        assert!(r.shown() < zoomed * 0.9);
    });
}

/// The zoom limits hold however hard the pinch.
#[test]
fn a_pinch_cannot_zoom_past_the_limits() {
    both_modes(|mut r| {
        for _ in 0..40 {
            r.frame(vec![Event::Zoom(2.0)]);
        }
        r.frame(vec![]);
        assert_eq!(r.shown(), MAX_SCALE);
        for _ in 0..60 {
            r.frame(vec![Event::Zoom(0.5)]);
        }
        r.frame(vec![]);
        assert_eq!(r.shown(), MIN_SCALE);
    });
}
