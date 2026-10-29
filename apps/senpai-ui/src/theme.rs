//! Visual design tokens, and the animated widgets built from them.
//!
//! Everything chrome-related lives here so the look is defined in one
//! place rather than as magic numbers scattered through `main.rs`.
//!
//! The vocabulary is borrowed from omen-space's GTK stylesheet and
//! translated to egui: capsule/rounded surfaces, 120-150ms state
//! transitions, a soft two-layer drop shadow, a muted accent reserved for
//! selection, and a segmented "track + sliding indicator" switcher in
//! place of a row of radio-ish buttons.
//!
//! One deliberate omission: nothing here animates *page content*. Fading
//! a page in on a turn would read as latency on top of a render path the
//! rest of the project works hard to keep instant, so chrome animates and
//! the document never does.

use eframe::egui::{self, pos2, vec2, Align2, Color32, Rect, Response, Rounding, Sense, Stroke};
use egui::epaint::Shadow;
use std::sync::atomic::{AtomicBool, Ordering};

// ── Radii ────────────────────────────────────────────────────────────────
//
// egui clamps a rounding to half the shorter side, so asking for an
// absurd radius is how you spell "capsule" -- the shape stays a true
// pill at any height without having to recompute it per widget.
pub const R_PILL: f32 = 999.0;
pub const R_CARD: f32 = 14.0;
/// Outer track of a segmented control; its indicator uses `R_TRACK - PAD`
/// so the inner and outer curves stay concentric.
pub const R_TRACK: f32 = 18.0;

/// Height of a [`Pill`], and so of anything that has to line up with the
/// row of them in the top bar. Tied to [`BAR_FONT`]: a pill is its label
/// plus padding, and the two have to move together or the capsule stops
/// matching the bar it sits in.
pub const PILL_HEIGHT: f32 = BAR_FONT + 12.0;

/// Type size for the top bar's buttons, and the floor for the readouts
/// beside them.
pub const BAR_FONT: f32 = 16.5;

/// Type size for the page counter. A step under the buttons -- it is a
/// readout, not a control -- but close enough to be read at a glance
/// from across the window.
pub const COUNTER_FONT: f32 = 15.0;

/// Vertical space between the bar's surface and the row of pills inside
/// it. Shared with the page capsule, which is why it is a constant: the
/// two are the same height only while they agree on this.
pub const BAR_MARGIN_Y: f32 = 5.0;
/// Horizontal counterpart. Wider than the vertical, because a capsule's
/// rounded ends eat into the horizontal room and not the vertical.
pub const BAR_MARGIN_X: f32 = 9.0;

// ── Durations (seconds) ──────────────────────────────────────────────────
//
// Long enough to read as motion, short enough never to sit between the
// user and the thing they clicked. Press is deliberately much faster than
// hover: a click should feel immediate, a hover can afford to ease in.
pub const T_HOVER: f32 = 0.12;
pub const T_PRESS: f32 = 0.055;
pub const T_STATE: f32 = 0.15;
pub const T_PANEL: f32 = 0.17;

// ── Palette ──────────────────────────────────────────────────────────────
//
// Both themes, inverted. The chrome used to be black-on-translucent
// whatever the mode, on the reasoning that the bar is the bar; but a
// white page under a black bar is two different documents' worth of
// contrast at the top of the window, and Light mode was only ever half
// applied while that was true.
//
// So the palette runs one way or the other and everything else is
// expressed against it: `veil` is a wash of the foreground colour, which
// is white over a dark chrome and black over a light one, so a hover
// highlight brightens in the dark and darkens in the light without any
// call site knowing which it is.
//
// Written premultiplied (r=g=b=a is opaque white laid over the backdrop
// at a/255) because that form is const-constructible.

/// Which way the palette runs. Set by [`install`], and deliberately the
/// only place it is decided -- the same reasoning as pinning egui's
/// theme preference there: asking a second source produced a window
/// whose chrome and whose text disagreed about the mode.
static DARK: AtomicBool = AtomicBool::new(true);

fn is_dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

fn pick(dark: Color32, light: Color32) -> Color32 {
    if is_dark() {
        dark
    } else {
        light
    }
}

const fn white(a: u8) -> Color32 {
    Color32::from_rgba_premultiplied(a, a, a, a)
}

const fn black(a: u8) -> Color32 {
    Color32::from_black_alpha(a)
}

/// A wash of whatever the foreground colour is: white over dark chrome,
/// black over light. Every incidental tint -- a hover fill, a hairline,
/// a track -- goes through this rather than naming a colour, so the two
/// themes cannot drift apart one highlight at a time.
fn veil(a: u8) -> Color32 {
    pick(white(a), black(a))
}

/// Muted blue, used *only* to mark selection. A reader's chrome should
/// recede; colour here is information, not decoration. Darker in Light
/// mode, where the pale blue had too little contrast against white to
/// carry white text on top of it.
pub fn accent() -> Color32 {
    pick(
        Color32::from_rgb(96, 165, 250),
        Color32::from_rgb(37, 99, 235),
    )
}

/// The surface every floating element sits on.
pub fn chrome() -> Color32 {
    pick(
        Color32::from_black_alpha(234),
        Color32::from_white_alpha(238),
    )
}

pub fn hairline() -> Color32 {
    pick(white(20), black(26))
}

pub fn text() -> Color32 {
    pick(white(236), black(228))
}

pub fn text_dim() -> Color32 {
    pick(white(150), black(160))
}

pub fn text_faint() -> Color32 {
    pick(white(96), black(110))
}

fn surface_hover() -> Color32 {
    veil(26)
}

/// The lifted surface of a selected control.
///
/// Not a veil, and that is the whole point of it being written out. A
/// raised surface is drawn over a contact shadow, and a translucent fill
/// lets that shadow through: in Night the shadow is black on black and
/// invisible, but in Light a veil let it darken the chip to gray(132)
/// against a gray(241) track -- a selected segment that looked pressed
/// into the surface rather than lifted off it. Opaque white sits on top
/// of its own shadow, which is what makes the shadow read as lift.
fn surface_raised() -> Color32 {
    pick(white(40), Color32::WHITE)
}

/// The same surface under the pointer. In Light it cannot go brighter
/// than the white it already is, so the hover is carried by the lift
/// alone.
fn surface_raised_hover() -> Color32 {
    pick(white(56), Color32::WHITE)
}

fn track_bg() -> Color32 {
    veil(14)
}

/// Two-layer shadow: a tight contact shadow plus a wide soft one. A single
/// blur either looks hard at the edge or muddy underneath; stacking them
/// is what gives omen-space's cards their lift.
pub fn soft_shadow(strength: f32) -> Shadow {
    Shadow {
        offset: vec2(0.0, 4.0),
        blur: 18.0,
        spread: 0.0,
        color: Color32::from_black_alpha((110.0 * strength) as u8),
    }
}

fn contact_shadow(strength: f32) -> Shadow {
    Shadow {
        offset: vec2(0.0, 1.0),
        blur: 3.0,
        spread: 0.0,
        color: Color32::from_black_alpha((90.0 * strength) as u8),
    }
}

// ── Global style ─────────────────────────────────────────────────────────

/// Installs the base style. Called on startup and whenever the theme
/// flips, since `Visuals` carries the light/dark decision.
pub fn install(ctx: &egui::Context, dark: bool) {
    // First, before anything below reads a colour: the chrome palette
    // runs off this, and `set_visuals` a few lines down is already one
    // of its readers.
    DARK.store(dark, Ordering::Relaxed);

    // Pin the preference before touching visuals, and it must be both:
    //
    // egui keeps a separate style per theme and serves whichever one
    // `Context::theme()` names. Left at its default that is the *system*
    // theme, so `set_visuals` writes into whichever slot happens to be
    // current at startup -- the Dark fallback, before the OS has been
    // heard from -- and the moment a Light system theme arrives egui
    // switches to the untouched Light slot. Under Wine that happened on
    // frame 0: `theme()` was Light while the app's own toggle said dark,
    // so anything reading `visuals()` got a stock light palette and
    // painted black text onto the dark background. Toggling the switch
    // wrote into the now-current slot, which is why it then fixed
    // itself.
    //
    // The app owns this choice, not the desktop: it has a Light/Night
    // switch of its own, and a reader that repaints itself because the
    // OS changed its mind is not doing what was asked.
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });

    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    // Any stock egui widget that slips through (a checkbox, a scroll
    // handle) should still be round rather than square, so the fallback
    // matches the hand-drawn widgets below.
    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.rounding = Rounding::same(R_PILL);
        w.expansion = 0.0;
    }

    visuals.window_rounding = Rounding::same(R_CARD);
    visuals.menu_rounding = Rounding::same(R_CARD);
    visuals.window_shadow = soft_shadow(1.0);
    visuals.popup_shadow = soft_shadow(1.0);
    visuals.selection.bg_fill = accent().gamma_multiply(0.40);
    visuals.selection.stroke = Stroke::new(1.0_f32, accent());

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = vec2(8.0, 8.0);
    style.spacing.button_padding = vec2(14.0, 7.0);
    style.spacing.interact_size.y = 30.0;
    // A floating, thin scrollbar: continuous mode should read as a page of
    // document, not a page inside a widget.
    style.spacing.scroll.floating = true;
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.handle_min_length = 32.0;
    ctx.set_style(style);
}

/// The translucent black surface every floating element sits on: the top
/// bar, its dropdown, the status pills.
pub fn chrome_frame(rounding: f32) -> egui::Frame {
    egui::Frame::none()
        .fill(chrome())
        .rounding(Rounding::same(rounding))
        .stroke(Stroke::new(1.0_f32, hairline()))
        .shadow(soft_shadow(1.0))
}

/// Section heading inside a dropdown. Uppercased and tracked-out by hand
/// -- egui has no letter-spacing, and spacing the characters is what makes
/// a 10px label read as a heading instead of as small body text.
pub fn section_label(ui: &mut egui::Ui, text: &str) {
    let spaced: String = text
        .to_uppercase()
        .chars()
        .flat_map(|c| [c, '\u{2009}'])
        .collect();
    ui.label(
        egui::RichText::new(spaced.trim_end())
            .color(text_faint())
            .size(10.0),
    );
}

/// Hairline divider, for separating groups inside a dropdown.
pub fn divider(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, Rounding::ZERO, veil(14));
    ui.add_space(4.0);
}

// ── Pill button ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PillStyle {
    /// Transparent until hovered. The default for menu entries.
    Ghost,
    /// Carries a faint surface at rest. For controls that must look
    /// clickable without being the active choice -- a Ghost pill sitting
    /// alone in a row just reads as a label.
    Quiet,
    /// Currently-active choice: a raised surface, as if lifted off the bar.
    Selected,
    /// The one primary action on screen.
    Accent,
}

pub struct Pill<'a> {
    text: &'a str,
    style: PillStyle,
    enabled: bool,
    min_width: f32,
    font_size: f32,
    align_left: bool,
}

impl<'a> Pill<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            style: PillStyle::Ghost,
            enabled: true,
            min_width: 0.0,
            font_size: BAR_FONT,
            align_left: false,
        }
    }

    pub fn style(mut self, style: PillStyle) -> Self {
        self.style = style;
        self
    }

    /// `Selected` when `on`, `Ghost` otherwise -- the common case for a
    /// toggle-like choice.
    pub fn selected(mut self, on: bool) -> Self {
        self.style = if on {
            PillStyle::Selected
        } else {
            PillStyle::Ghost
        };
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn min_width(mut self, w: f32) -> Self {
        self.min_width = w;
        self
    }

    pub fn small(mut self) -> Self {
        self.font_size = 12.0;
        self
    }

    /// Pins the label to the left edge instead of centring it.
    ///
    /// For a column of full-width pills that are a *list* -- recent
    /// documents, say -- centring leaves every label starting at a
    /// different x, which is much harder to scan than a flush edge.
    /// Buttons stay centred; lists get this.
    pub fn align_left(mut self) -> Self {
        self.align_left = true;
        self
    }

    pub fn show(self, ui: &mut egui::Ui) -> Response {
        let font = egui::FontId::proportional(self.font_size);
        let text_w = ui
            .fonts(|f| f.layout_no_wrap(self.text.to_owned(), font.clone(), Color32::WHITE))
            .size()
            .x;

        // Padding kept tight on purpose: a pill is a label with enough
        // room to be pressed, not a slab with a label in it. The height
        // floor is what lines every pill in the bar up with its
        // neighbours whatever size their text is.
        let desired = vec2(
            (text_w + 24.0).max(self.min_width),
            (self.font_size + 11.0).max(PILL_HEIGHT),
        );
        let sense = if self.enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(desired, sense);
        if !ui.is_rect_visible(rect) {
            return response;
        }

        let ctx = ui.ctx();
        let hover = ctx.animate_bool_with_time(
            response.id.with("hov"),
            self.enabled && response.hovered(),
            T_HOVER,
        );
        let press = ctx.animate_bool_with_time(
            response.id.with("prs"),
            response.is_pointer_button_down_on(),
            T_PRESS,
        );

        // Press sinks the pill a little instead of flashing a different
        // colour. It reads as physical, and it works the same whatever
        // the pill's resting fill happens to be.
        let rect = rect.shrink(press * 1.5);
        let rounding = Rounding::same(R_PILL);

        let (fill, text_col, lift) = match self.style {
            PillStyle::Ghost => (
                Color32::TRANSPARENT.lerp_to_gamma(surface_hover(), hover),
                text_dim().lerp_to_gamma(text(), hover),
                0.0,
            ),
            PillStyle::Quiet => (
                veil(16).lerp_to_gamma(surface_hover(), hover),
                text_dim().lerp_to_gamma(text(), hover),
                0.0,
            ),
            PillStyle::Selected => (
                surface_raised().lerp_to_gamma(surface_raised_hover(), hover),
                text(),
                0.5 + 0.5 * hover,
            ),
            PillStyle::Accent => (
                accent().gamma_multiply(0.92).lerp_to_gamma(accent(), hover),
                Color32::WHITE,
                0.6 + 0.4 * hover,
            ),
        };

        if lift > 0.0 {
            let strength = lift * (1.0 - press * 0.6);
            ui.painter()
                .add(soft_shadow(strength * 0.5).as_shape(rect, rounding));
            ui.painter()
                .add(contact_shadow(strength).as_shape(rect, rounding));
        }

        ui.painter().rect_filled(rect, rounding, fill);

        // A hairline that fades in with hover: enough to separate a ghost
        // pill from the bar behind it without drawing a permanent box.
        if matches!(self.style, PillStyle::Ghost | PillStyle::Quiet) && hover > 0.0 {
            ui.painter().rect_stroke(
                rect,
                rounding,
                Stroke::new(1.0_f32, veil((22.0 * hover) as u8)),
            );
        }

        // A disabled pill still gets an outline: "this is a control that
        // is off" rather than "this is a dim piece of text".
        if !self.enabled {
            ui.painter()
                .rect_stroke(rect, rounding, Stroke::new(1.0_f32, veil(16)));
        }
        let text_col = if self.enabled { text_col } else { text_faint() };
        let (anchor, pos) = if self.align_left {
            (
                Align2::LEFT_CENTER,
                pos2(rect.left() + 14.0, rect.center().y),
            )
        } else {
            (Align2::CENTER_CENTER, rect.center())
        };
        ui.painter().text(pos, anchor, self.text, font, text_col);

        response
    }
}

// ── Segmented control ────────────────────────────────────────────────────

/// A track with a sliding indicator, as opposed to N independent buttons.
///
/// The indicator position is animated as a *continuous* value rather than
/// a boolean per segment, so it glides between any two choices and keeps
/// its momentum if the selection changes mid-flight.
///
/// Returns the newly clicked index, if any.
pub fn segmented(
    ui: &mut egui::Ui,
    id_salt: &str,
    labels: &[&str],
    selected: usize,
) -> Option<usize> {
    if labels.is_empty() {
        return None;
    }

    let font = egui::FontId::proportional(11.5);
    let widest = labels
        .iter()
        .map(|l| {
            ui.fonts(|f| f.layout_no_wrap((*l).to_owned(), font.clone(), Color32::WHITE))
                .size()
                .x
        })
        .fold(0.0_f32, f32::max);

    const PAD: f32 = 3.0;
    let seg = vec2(widest + 26.0, 26.0);
    let desired = vec2(seg.x * labels.len() as f32 + PAD * 2.0, seg.y + PAD * 2.0);
    let (track, _) = ui.allocate_exact_size(desired, Sense::hover());
    if !ui.is_rect_visible(track) {
        return None;
    }

    ui.painter()
        .rect_filled(track, Rounding::same(R_TRACK), track_bg());
    let inner = track.shrink(PAD);
    let id = ui.id().with(id_salt);

    let t = ui
        .ctx()
        .animate_value_with_time(id.with("slide"), selected as f32, T_STATE);

    let indicator = Rect::from_min_size(
        pos2(inner.left() + t * seg.x, inner.top()),
        vec2(seg.x, inner.height()),
    );
    let ind_rounding = Rounding::same(R_TRACK - PAD);
    ui.painter()
        .add(contact_shadow(1.0).as_shape(indicator, ind_rounding));
    ui.painter()
        .rect_filled(indicator, ind_rounding, surface_raised());

    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        let rect = Rect::from_min_size(
            pos2(inner.left() + i as f32 * seg.x, inner.top()),
            vec2(seg.x, inner.height()),
        );
        let response = ui.interact(rect, id.with(i), Sense::click());
        if response.clicked() {
            clicked = Some(i);
        }

        // Brighten by proximity to the indicator, not by `i == selected`,
        // so the label lights up in step with the slide rather than
        // snapping ahead of it.
        let nearness = 1.0 - (t - i as f32).abs().clamp(0.0, 1.0);
        let hover =
            ui.ctx()
                .animate_bool_with_time(id.with(("hov", i)), response.hovered(), T_HOVER);
        let col = text_dim().lerp_to_gamma(text(), nearness.max(hover * 0.55));

        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            *label,
            font.clone(),
            col,
        );
    }

    clicked
}

// ── Toggle switch ────────────────────────────────────────────────────────

/// Replaces egui's square checkbox. The knob slides and the track colours
/// toward the accent, which makes the state readable at a glance from the
/// shape alone rather than from a tick that has to be looked at.
pub fn switch(ui: &mut egui::Ui, on: &mut bool) -> Response {
    let size = vec2(38.0, 21.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if !ui.is_rect_visible(rect) {
        return response;
    }

    let ctx = ui.ctx();
    let t = ctx.animate_bool_with_time(response.id.with("on"), *on, T_STATE);
    let hover = ctx.animate_bool_with_time(response.id.with("hov"), response.hovered(), T_HOVER);

    let track = veil(22).lerp_to_gamma(accent(), t);
    let rounding = Rounding::same(rect.height() / 2.0);
    ui.painter().rect_filled(rect, rounding, track);
    if hover > 0.0 {
        ui.painter().rect_stroke(
            rect,
            rounding,
            Stroke::new(1.0_f32, veil((30.0 * hover) as u8)),
        );
    }

    let r = rect.height() / 2.0 - 3.0;
    let cx = egui::lerp((rect.left() + r + 3.0)..=(rect.right() - r - 3.0), t);
    let center = pos2(cx, rect.center().y);
    ui.painter().add(contact_shadow(0.8).as_shape(
        Rect::from_center_size(center, vec2(r * 2.0, r * 2.0)),
        Rounding::same(r),
    ));
    ui.painter().circle_filled(center, r, Color32::WHITE);

    response
}

/// A labelled row: text on the left, switch pushed to the right edge.
pub fn switch_row(ui: &mut egui::Ui, label: &str, on: &mut bool) -> Response {
    let mut response = None;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(text()).size(12.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            response = Some(switch(ui, on));
        });
    });
    response.expect("switch_row always adds a switch")
}

// ── Status pill ──────────────────────────────────────────────────────────

/// A small floating capsule, used for transient status ("rendering…") and
/// the page counter. `opacity` is applied by the caller's animation so the
/// pill can fade rather than pop.
pub fn status_pill(
    ctx: &egui::Context,
    id: &str,
    anchor: Align2,
    offset: egui::Vec2,
    opacity: f32,
    text: &str,
) {
    if opacity <= 0.001 {
        return;
    }
    egui::Area::new(egui::Id::new(id))
        .anchor(anchor, offset)
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_opacity(opacity);
            chrome_frame(R_PILL)
                .inner_margin(egui::Margin::symmetric(13.0, 7.0))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(text).color(text_dim()).size(11.5));
                });
        });
}

// ── Drag-and-drop overlay ────────────────────────────────────────────────

/// The veil shown while files are being dragged over the window.
///
/// Painted straight onto its own layer above `Order::Foreground` rather
/// than into a panel, for two reasons: it has to cover the page *and*
/// the top bar (during a drag, the only question on screen is whether
/// letting go will do something), and update() has more than one exit
/// path, so a single unconditional call at the top beats repeating it
/// at each one.
///
/// `t` is the usual 0..1 fade. At 0 nothing is drawn at all, so a
/// fully-faded overlay costs one comparison.
pub fn drop_overlay(ctx: &egui::Context, t: f32, label: Option<&str>) {
    if t <= 0.001 {
        return;
    }
    let screen = ctx.screen_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("drop_overlay"),
    ));

    painter.rect_filled(
        screen,
        Rounding::ZERO,
        Color32::from_black_alpha((150.0 * t) as u8),
    );

    // The border eases inward as it appears, which reads as the window
    // accepting the drag rather than a rectangle blinking on.
    let inset = screen.shrink(16.0 + 10.0 * (1.0 - t));
    painter.rect_stroke(
        inset,
        Rounding::same(R_CARD + 6.0),
        Stroke::new(2.0_f32, accent().gamma_multiply(t * 0.85)),
    );

    let title = painter.layout_no_wrap(
        "Drop to open".to_owned(),
        egui::FontId::proportional(15.0),
        text().gamma_multiply(t),
    );
    let sub = label.map(|l| {
        painter.layout_no_wrap(
            l.to_owned(),
            egui::FontId::proportional(11.5),
            text_dim().gamma_multiply(t),
        )
    });

    let text_w = title.size().x.max(sub.as_ref().map_or(0.0, |g| g.size().x));
    let card = Rect::from_center_size(
        screen.center(),
        vec2(
            (text_w + 48.0).max(200.0),
            if sub.is_some() { 74.0 } else { 56.0 },
        ),
    );
    let rounding = Rounding::same(R_CARD);
    painter.add(soft_shadow(t).as_shape(card, rounding));
    painter.rect_filled(card, rounding, chrome().gamma_multiply(t));
    painter.rect_stroke(
        card,
        rounding,
        Stroke::new(1.0_f32, hairline().gamma_multiply(t)),
    );

    let title_y = if sub.is_some() {
        card.center().y - 10.0
    } else {
        card.center().y
    };
    painter.galley(
        pos2(
            card.center().x - title.size().x / 2.0,
            title_y - title.size().y / 2.0,
        ),
        title,
        Color32::WHITE,
    );
    if let Some(sub) = sub {
        painter.galley(
            pos2(card.center().x - sub.size().x / 2.0, card.center().y + 6.0),
            sub,
            Color32::WHITE,
        );
    }
}

// ── Page counter ─────────────────────────────────────────────────────────

/// The counter as it appears inside the top bar: the glyphs alone, in a
/// slot the bar has made room for.
///
/// The slot is sized by the caller on the widest text the counter will
/// ever hold rather than on what it holds now, so the number does not
/// shuffle sideways on the turn from page 9 to page 10.
pub fn counter_label(ui: &mut egui::Ui, text: &str, slot_w: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(slot_w, PILL_HEIGHT), Sense::hover());
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(COUNTER_FONT),
        text_dim(),
    );
}

/// The counter as it appears on its own, with the bar hidden: a capsule
/// centred at the top of the window.
///
/// Centred rather than standing where the bar's own counter stands. The
/// two are different things -- one is a readout on a bar full of
/// controls, the other is the only thing on screen -- and the lone one
/// belongs in the middle, where nothing else is competing for the eye.
///
/// Its height is the bar's by construction: the same [`BAR_MARGIN_Y`]
/// around a content box of exactly [`PILL_HEIGHT`], which is what the
/// bar's own buttons stand at.
///
/// Painted above `Order::Foreground`, because egui raises a layer to the
/// top when it is clicked and sharing an order with the bar meant a
/// single click on a menu could leave the bar permanently in front.
pub fn page_capsule(ctx: &egui::Context, offset_y: f32, text: &str, t: f32) {
    if t <= 0.001 {
        return;
    }
    egui::Area::new(egui::Id::new("page_capsule"))
        .anchor(Align2::CENTER_TOP, vec2(0.0, offset_y))
        .order(egui::Order::Tooltip)
        // Never takes a click: it is a readout, and swallowing presses
        // would punch a hole in the middle of the page.
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_opacity(t);
            chrome_frame(R_PILL)
                .inner_margin(egui::Margin::symmetric(14.0, BAR_MARGIN_Y))
                .show(ui, |ui| {
                    let font = egui::FontId::proportional(COUNTER_FONT);
                    // Sized to the text: a four-digit count gets the room
                    // it needs and a single-digit one leaves no gap.
                    let width = ui
                        .fonts(|f| f.layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE))
                        .size()
                        .x;
                    counter_label(ui, text, width);
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Theme, ThemePreference};
    use std::sync::Mutex;

    /// The palette is process-global, so the tests that move it have to
    /// take turns. Without this they are the same race the CLI fixtures
    /// had: parallel threads writing one thing and reading it back,
    /// green here and intermittent on a slower machine.
    ///
    /// Poisoning is stepped over rather than propagated -- one failing
    /// test should fail alone, not take the rest of the file with it.
    static PALETTE: Mutex<()> = Mutex::new(());

    fn palette_guard() -> std::sync::MutexGuard<'static, ()> {
        PALETTE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pins the bug behind v0.2.1: egui keeps one style per theme and
    /// serves whichever `Context::theme()` names. Left on the system
    /// preference, a desktop reporting the opposite theme -- Wine
    /// reports Light on frame 0 -- served a stock palette while the
    /// app's own switch said otherwise, which painted black Markdown
    /// text onto the dark background until the switch was toggled.
    /// The chrome used to be black in both themes, so Light mode left
    /// the top bar and the page capsule dark. Both now follow the
    /// switch, and in opposite directions.
    #[test]
    fn the_chrome_palette_follows_the_mode() {
        let _serialised = palette_guard();
        let ctx = egui::Context::default();

        install(&ctx, true);
        let (dark_chrome, dark_text) = (chrome(), text());

        install(&ctx, false);
        let (light_chrome, light_text) = (chrome(), text());

        assert_ne!(dark_chrome, light_chrome, "the surface must change");
        assert_ne!(dark_text, light_text, "the text must change with it");

        // And in opposite directions: a dark surface carries light text,
        // a light surface dark text.
        let lum = |c: Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        assert!(
            lum(dark_chrome) < lum(dark_text),
            "Night: text should be lighter than the surface"
        );
        assert!(
            lum(light_chrome) > lum(light_text),
            "Light: text should be darker than the surface"
        );
    }

    /// `veil` is what every incidental tint goes through, so this is the
    /// one place the two themes could silently drift apart.
    #[test]
    fn a_veil_washes_toward_the_foreground_of_whichever_theme_is_on() {
        let _serialised = palette_guard();
        let ctx = egui::Context::default();

        install(&ctx, true);
        let dark_veil = veil(40);
        install(&ctx, false);
        let light_veil = veil(40);

        // Over a dark chrome a veil lightens; over a light one it darkens.
        assert!(dark_veil.r() > 0, "Night veils lighten");
        assert_eq!(light_veil.r(), 0, "Light veils darken");
        assert_eq!(dark_veil.a(), light_veil.a(), "same strength either way");
    }

    #[test]
    fn install_overrides_whatever_the_desktop_asked_for() {
        let _serialised = palette_guard();
        for (dark, want) in [(true, Theme::Dark), (false, Theme::Light)] {
            let ctx = egui::Context::default();
            // Stand in for a desktop insisting on the opposite theme.
            ctx.options_mut(|o| {
                o.theme_preference = ThemePreference::System;
                o.fallback_theme = if dark { Theme::Light } else { Theme::Dark };
            });

            install(&ctx, dark);

            assert_eq!(ctx.theme(), want, "install(dark={dark}) must pin the theme");
            assert_eq!(
                ctx.style().visuals.dark_mode,
                dark,
                "the style actually served must be the one install wrote"
            );
        }
    }
}
