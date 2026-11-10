//! Continuous scroll: every page of a PDF in one column.
//!
//! Split in two so the half that can be wrong quietly is the half that
//! can be tested without a window:
//!
//! - [`Layout`] is arithmetic. Where each page sits in the column, which
//!   pages a viewport covers, where a page turn lands. No egui state, no
//!   textures.
//! - [`View`] is the scroll area around it. It owns the reading position
//!   and hands each visible page's rectangle to a callback, which is the
//!   only place pixels come into it.
//!
//! The layout is exact from the first frame. Every page's size is read
//! when the document opens (about 9us a page, measured: 11ms for a
//! 1278-page textbook), so a row never changes height when its render
//! arrives. The version this replaces laid rows out from an estimate and
//! corrected each one as it was rendered, which moved everything below
//! it -- including whatever was being read.
//!
//! A reading position is an [`Anchor`] -- a page and how far down and
//! across it -- rather than a pixel offset. An offset means something
//! only for one zoom level and one window size; an anchor survives
//! both. That is what lets a resize leave the same line at the top of
//! the window, and a pinch leave the word under the pointer under the
//! pointer.
//!
//! Pages are laid out at their real size for the zoom level. One wider
//! than the window makes the column wider than the window, and the view
//! scrolls sideways to reach the rest of it; nothing is shrunk to fit.
//! Fitting is a zoom level the caller chooses (see [`fit_scale`]), not
//! something the layout does behind its back.
//!
//! The paged view is this same view, shown one page at a time.

use crate::scroll::Scroller;
use eframe::egui::{self, pos2, vec2, Pos2, Rect};
use senpai_core::model::PageSize;
use std::ops::Range;

/// Space between pages, and above the first and below the last.
pub const PAGE_GAP: f32 = 12.0;
/// Space kept clear on each side of the widest page.
pub const SIDE_MARGIN: f32 = 12.0;
/// US Letter, standing in for a page whose own size is unusable (zero,
/// negative or not a number). The row has to have *some* height, and a
/// wrong one is better than a division by zero.
pub const FALLBACK_PAGE_SIZE: PageSize = PageSize {
    width_pt: 612.0,
    height_pt: 792.0,
};

/// A point on a page, which stays the same point at any zoom level and
/// in any window.
///
/// `frac` is measured down from the page's top edge in page heights,
/// `xfrac` across from its left edge in page widths. Both may fall
/// outside 0..1 for a point in the gap beside or below a page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub page: usize,
    pub frac: f32,
    pub xfrac: f32,
}

impl Anchor {
    /// The top of a page, at its horizontal middle.
    pub fn top_of(page: usize) -> Self {
        Self {
            page,
            frac: 0.0,
            xfrac: 0.5,
        }
    }
}

/// Which way a page is fitted to the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// The whole page visible at once.
    Page,
    /// The page as wide as the window, however tall that makes it.
    Width,
}

/// The zoom level at which a page of `size` fits a window of `avail`.
///
/// Rounded down to a thousandth. Unrounded, a window dragged one pixel
/// wider is a new zoom level, and every zoom level is a fresh render of
/// every page on screen. Down rather than to nearest, so that rounding
/// can never push a fitted page a fraction of a point past the edge and
/// summon a scrollbar for it.
pub fn fit_scale(fit: Fit, size: PageSize, avail: egui::Vec2) -> f64 {
    let size = usable(size).unwrap_or(FALLBACK_PAGE_SIZE);
    let by_width = (avail.x - 2.0 * SIDE_MARGIN).max(1.0) as f64 / size.width_pt;
    let by_height = (avail.y - 2.0 * PAGE_GAP).max(1.0) as f64 / size.height_pt;
    let scale = match fit {
        Fit::Width => by_width,
        Fit::Page => by_width.min(by_height),
    };
    (scale * 1000.0).floor() / 1000.0
}

/// The size to fit a whole document by: as wide as its widest page and
/// as tall as its tallest, so that fitting it fits every page.
pub fn bounding_size(pages: &[PageSize]) -> PageSize {
    let mut bound = PageSize {
        width_pt: 0.0,
        height_pt: 0.0,
    };
    for size in pages.iter().filter_map(|p| usable(*p)) {
        bound.width_pt = bound.width_pt.max(size.width_pt);
        bound.height_pt = bound.height_pt.max(size.height_pt);
    }
    usable(bound).unwrap_or(FALLBACK_PAGE_SIZE)
}

fn usable(size: PageSize) -> Option<PageSize> {
    let ok = |v: f64| v.is_finite() && v >= 1.0;
    (ok(size.width_pt) && ok(size.height_pt)).then_some(size)
}

/// Where every page sits, for one zoom level and one window size.
///
/// Coordinates are in points from the top-left of the column. The
/// column is as wide as the window or the widest page, whichever is
/// more, and each page is centred in it. A document shorter than the
/// window is centred vertically too, which is what puts a single fitted
/// page in the middle of the window rather than at the top of it.
pub struct Layout {
    tops: Vec<f32>,
    sizes: Vec<egui::Vec2>,
    width: f32,
    total: f32,
}

impl Layout {
    pub fn new(pages: &[PageSize], scale: f64, avail: egui::Vec2) -> Self {
        let mut tops = Vec::with_capacity(pages.len());
        let mut sizes = Vec::with_capacity(pages.len());
        let mut widest = 0.0_f32;
        let mut y = PAGE_GAP;
        for page in pages {
            let size = usable(*page).unwrap_or(FALLBACK_PAGE_SIZE);
            let w = ((size.width_pt * scale) as f32).max(1.0);
            let h = ((size.height_pt * scale) as f32).max(1.0);
            tops.push(y);
            sizes.push(vec2(w, h));
            widest = widest.max(w);
            y += h + PAGE_GAP;
        }
        let mut total = y;
        if total < avail.y {
            let pad = (avail.y - total) / 2.0;
            for top in &mut tops {
                *top += pad;
            }
            total = avail.y;
        }
        Self {
            tops,
            sizes,
            width: avail.x.max(widest + 2.0 * SIDE_MARGIN),
            total,
        }
    }

    /// Height of the whole column, margins included.
    pub fn total(&self) -> f32 {
        self.total
    }

    /// Width of the whole column: the window's, unless a page is wider.
    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn is_empty(&self) -> bool {
        self.tops.is_empty()
    }

    fn left(&self, page: usize) -> f32 {
        (self.width - self.sizes[page].x) / 2.0
    }

    /// The page whose row `y` falls in. A row runs from a page's top
    /// edge to the next page's, so the gap below a page belongs to it.
    /// Anything above the first page counts as the first.
    pub fn page_at(&self, y: f32) -> usize {
        self.tops.partition_point(|&t| t <= y).saturating_sub(1)
    }

    /// The pages with any part between `top` and `bottom`.
    pub fn visible(&self, top: f32, bottom: f32) -> Range<usize> {
        let len = self.tops.len();
        if len == 0 || self.tops[0] > bottom {
            return 0..0;
        }
        let mut first = self.page_at(top);
        if self.tops[first] + self.sizes[first].y <= top {
            // `top` is in the gap under this page: nothing of it shows.
            first += 1;
        }
        let end = self.page_at(bottom) + 1;
        first.min(len)..end.max(first).min(len)
    }

    /// The page taking up the most of the viewport, the earlier one on a
    /// tie. This is the page the counter reports.
    ///
    /// Not simply the page at the top: with a sliver of one page left
    /// above a nearly whole next one, the reader is on the next one. And
    /// not the page at the middle either: zoomed out, a page turn puts
    /// the wanted page at the top with others below it, and the middle
    /// of the window would belong to one of those.
    pub fn most_visible(&self, top: f32, bottom: f32) -> usize {
        let mut best = (self.page_at(top), 0.0_f32);
        for page in self.visible(top, bottom) {
            let page_top = self.tops[page];
            let page_bottom = page_top + self.sizes[page].y;
            let shown = page_bottom.min(bottom) - page_top.max(top);
            // A whole pixel more, so two fully visible pages of the same
            // nominal size do not trade places on rounding error.
            if shown > best.1 + 1.0 {
                best = (page, shown);
            }
        }
        best.0
    }

    /// Where in the column `anchor` is.
    pub fn point_of(&self, anchor: Anchor) -> egui::Vec2 {
        if self.is_empty() {
            return egui::Vec2::ZERO;
        }
        let page = anchor.page.min(self.tops.len() - 1);
        let size = self.sizes[page];
        vec2(
            self.left(page) + anchor.xfrac * size.x,
            self.tops[page] + anchor.frac * size.y,
        )
    }

    /// The inverse of [`Layout::point_of`].
    pub fn anchor_at(&self, point: egui::Vec2) -> Anchor {
        if self.is_empty() {
            return Anchor::top_of(0);
        }
        let page = self.page_at(point.y);
        let size = self.sizes[page];
        Anchor {
            page,
            frac: (point.y - self.tops[page]) / size.y,
            xfrac: (point.x - self.left(page)) / size.x,
        }
    }

    /// Where a "next page" key lands from `offset`: the top of the first
    /// page that starts below the top of the window. At the last page
    /// there is nowhere further, and the offset comes back unchanged.
    pub fn next_page_offset(&self, offset: f32) -> f32 {
        let next = self.tops.partition_point(|&t| t - PAGE_GAP <= offset + 1.0);
        match self.tops.get(next) {
            Some(top) => top - PAGE_GAP,
            None => offset,
        }
    }

    /// Where a "previous page" key lands: the top of the page being
    /// read if the window is part-way down it, else the page before.
    pub fn prev_page_offset(&self, offset: f32) -> f32 {
        let before = self.tops.partition_point(|&t| t - PAGE_GAP < offset - 1.0);
        match before {
            0 => 0.0,
            n => self.tops[n - 1] - PAGE_GAP,
        }
    }

    /// The page's rectangle on screen, given where the column's own
    /// top-left corner currently is. `origin` is what carries the scroll
    /// position: it moves up as the reader scrolls down.
    pub fn page_rect(&self, page: usize, origin: Pos2) -> Rect {
        Rect::from_min_size(
            pos2(origin.x + self.left(page), origin.y + self.tops[page]),
            self.sizes[page],
        )
    }
}

/// The order to render pages in: what is on screen first, then what the
/// reader is heading towards, then what they just left.
///
/// The order matters because the worker renders one page at a time and
/// the queue is replaced whenever the window moves. Whatever is first is
/// what gets rendered; everything after it is a hope.
pub fn render_order(
    visible: Range<usize>,
    page_count: usize,
    buffer: usize,
    forward: bool,
) -> Vec<usize> {
    let visible = visible.start.min(page_count)..visible.end.min(page_count);
    let ahead = visible.end..(visible.end + buffer).min(page_count);
    let behind = (visible.start.saturating_sub(buffer)..visible.start).rev();
    let mut order = Vec::with_capacity(visible.len() + 2 * buffer);
    if forward {
        order.extend(visible);
        order.extend(ahead);
        order.extend(behind);
    } else {
        order.extend(visible.rev());
        order.extend(behind);
        order.extend(ahead);
    }
    order
}

/// What one frame of the view showed.
pub struct Shown {
    /// Pages with any part on screen.
    pub visible: Range<usize>,
    /// The page the counter should report.
    pub current_page: usize,
    /// Whether the reader last moved down the document rather than up.
    pub forward: bool,
    /// Whether there is more of the column than fits the window's
    /// height -- that is, whether scrolling vertically does anything.
    pub overflows: bool,
}

/// Identifies a layout: zoom, window size, and which pages. If none of
/// them has changed, neither has any page's position.
type LayoutKey = (u64, u32, u32, usize, usize);

/// The scroll area, and the reading position in it.
pub struct View {
    /// Salt for the scroll area's id, so the paged and the continuous
    /// view do not share one scroll position between them.
    name: &'static str,
    offset: egui::Vec2,
    forward: bool,
    layout: Option<(LayoutKey, Layout)>,
    /// A point to put somewhere on the next frame: the anchor, and where
    /// in the window it should end up. `None` for the place means where
    /// a page turn puts a page's top: centred, one gap below the top of
    /// the window, so its margin shows above it.
    pending: Option<(Anchor, Option<egui::Vec2>)>,
    /// Page turns asked for since the last frame; negative is backwards.
    page_steps: i32,
    /// Finer movement asked for since the last frame, in window heights.
    scroll_screens: f32,
    /// Wheel, touchpad and key motion in progress. See `scroll.rs`.
    scroller: Scroller,
}

impl View {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            offset: egui::Vec2::ZERO,
            forward: true,
            layout: None,
            pending: Some((Anchor::top_of(0), None)),
            page_steps: 0,
            scroll_screens: 0.0,
            scroller: Scroller::default(),
        }
    }

    /// Puts the top of `page` at the top of the window on the next
    /// frame.
    pub fn go_to_page(&mut self, page: usize) {
        self.pending = Some((Anchor::top_of(page), None));
    }

    /// Where the view is scrolled to.
    #[cfg(test)]
    pub fn offset(&self) -> egui::Vec2 {
        self.offset
    }

    /// Moves by whole pages on the next frame.
    pub fn turn_pages(&mut self, steps: i32) {
        self.page_steps += steps;
    }

    /// Moves by a fraction of the window's height on the next frame.
    pub fn scroll_by_screens(&mut self, screens: f32) {
        self.scroll_screens += screens;
    }

    /// Lays the column out into `ui` and calls `paint` for each page on
    /// screen with the page's index and its rectangle.
    ///
    /// `focus` is the point on screen that should stay still if the zoom
    /// level has changed since the last frame -- the pointer, for a
    /// pinch. Without one, what is at the top of the window stays there.
    /// `line_pt` is how far one wheel notch scrolls.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        pages: &[PageSize],
        scale: f64,
        focus: Option<Pos2>,
        line_pt: f32,
        mut paint: impl FnMut(&egui::Painter, usize, Rect),
    ) -> Shown {
        let avail = ui.available_size();
        let window = Rect::from_min_size(ui.max_rect().min, avail);
        // Where a page's top edge rests after a page turn. Also the point
        // held still by a zoom that has no pointer to go by -- and for
        // the same reason: it is the one point that is exactly on a page
        // edge after a turn, so holding it keeps that edge exactly
        // where it was, where holding the window's own top edge would
        // hold a point in the gap, which does not scale with the page.
        let top_centre = vec2(avail.x / 2.0, PAGE_GAP);
        let key: LayoutKey = (
            scale.to_bits(),
            avail.x.to_bits(),
            avail.y.to_bits(),
            pages.as_ptr() as usize,
            pages.len(),
        );
        if self.layout.as_ref().map(|(k, _)| *k) != Some(key) {
            // Every page is about to move. Before the old layout goes,
            // ask it what is at the point that should stay put, so the
            // new one can be scrolled to put the same thing there.
            if self.pending.is_none() {
                let hold = focus
                    .filter(|p| window.contains(*p))
                    .map_or(top_centre, |p| p - window.min);
                if let Some((_, old)) = &self.layout {
                    self.pending = Some((old.anchor_at(self.offset + hold), Some(hold)));
                }
            }
            self.layout = Some((key, Layout::new(pages, scale, avail)));
        }
        let layout = &self.layout.as_ref().expect("set just above").1;
        let max_offset = vec2(
            (layout.width() - avail.x).max(0.0),
            (layout.total() - avail.y).max(0.0),
        );

        let mut target = self.offset;
        let mut forced = false;
        if let Some((anchor, place)) = self.pending.take() {
            target = layout.point_of(anchor) - place.unwrap_or(top_centre);
            forced = true;
        }
        let steps = std::mem::take(&mut self.page_steps);
        for _ in 0..steps.unsigned_abs() {
            target.y = if steps > 0 {
                layout.next_page_offset(target.y)
            } else {
                layout.prev_page_offset(target.y)
            };
            forced = true;
        }
        if forced {
            // Sent somewhere on purpose: whatever glide or coast was in
            // progress belonged to the place just left.
            self.scroller.stop();
        }
        let screens = std::mem::take(&mut self.scroll_screens);
        if screens != 0.0 {
            self.scroller.nudge(screens * avail.y);
        }

        // The wheel is taken here, before the scroll area can have it.
        // egui would turn it into its own short eased step; `Scroller`
        // turns it into something that feels like a browser. The scroll
        // area is left with the scrollbars, sideways scrolling and
        // drag-to-pan, which it does well.
        let (now, dt, wheel) = crate::scroll::take_wheel(ui, line_pt);
        let moved = self.scroller.step(now, dt, &wheel);
        if moved != 0.0 {
            target.y += moved;
            forced = true;
            if target.y <= 0.0 || target.y >= max_offset.y {
                // An end of the document. Anything still owed would
                // otherwise sit there and be paid out the moment the
                // reader scrolled back the other way.
                self.scroller.stop();
            }
        }
        if self.scroller.is_active() {
            // Motion without input: egui only repaints when something
            // happens, and a glide is something happening.
            ui.ctx().request_repaint();
        }

        let mut area = egui::ScrollArea::both()
            .id_salt(self.name)
            .auto_shrink([false, false]);
        if forced {
            area = area.scroll_offset(target.clamp(egui::Vec2::ZERO, max_offset));
        }
        let out = area.show_viewport(ui, |ui, viewport| {
            // The column's top-left corner *on screen*. egui has already
            // moved it by the scroll offset, so adding a page's position
            // in the column to it gives where that page is now. The
            // broken version of this view placed pages at their column
            // position as if it were a screen position, which is right
            // only while nothing has been scrolled.
            let origin = ui.max_rect().min;
            ui.allocate_rect(
                Rect::from_min_size(origin, vec2(layout.width(), layout.total())),
                egui::Sense::hover(),
            );
            // Shrunk by a point at each end, so a page with less than
            // that showing is not shown. A fitted page is a fraction of
            // a point shorter than the room it was fitted to -- the fit
            // is rounded down -- which leaves exactly that much of the
            // next page inside the window: a one-pixel line of somebody
            // else's paper along the bottom edge of a page that was
            // asked to fill the screen on its own.
            let visible = layout.visible(viewport.min.y + 1.0, viewport.max.y - 1.0);
            for page in visible.clone() {
                paint(ui.painter(), page, layout.page_rect(page, origin));
            }
            visible
        });

        let offset = out.state.offset;
        if offset.y != self.offset.y {
            self.forward = offset.y > self.offset.y;
        }
        self.offset = offset;
        Shown {
            visible: out.inner,
            current_page: layout.most_visible(offset.y, offset.y + avail.y),
            forward: self.forward,
            overflows: max_offset.y > 0.5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LETTER: PageSize = PageSize {
        width_pt: 612.0,
        height_pt: 792.0,
    };

    /// A window 1000 wide and 600 tall.
    const WINDOW: egui::Vec2 = vec2(1000.0, 600.0);

    /// Ten letter pages at 1:1: rows are 792 tall, 804 apart, the first
    /// at 12.
    fn ten() -> Layout {
        Layout::new(&[LETTER; 10], 1.0, WINDOW)
    }

    /// The scroll offset that puts the top of `page` at the top of the
    /// window.
    fn top_offset(l: &Layout, page: usize) -> f32 {
        l.point_of(Anchor::top_of(page)).y - PAGE_GAP
    }

    #[test]
    fn pages_stack_with_a_gap_between_and_a_margin_at_each_end() {
        let l = ten();
        assert_eq!(l.tops[0], PAGE_GAP);
        assert_eq!(l.tops[1], PAGE_GAP + 792.0 + PAGE_GAP);
        assert_eq!(l.total(), PAGE_GAP + 10.0 * (792.0 + PAGE_GAP));
    }

    /// Nothing is shrunk to fit any more: a page wider than the window
    /// keeps its size and the column grows to hold it, margins and all.
    #[test]
    fn a_page_wider_than_the_window_widens_the_column_instead_of_shrinking() {
        let l = Layout::new(&[LETTER], 2.0, vec2(636.0, 600.0));
        assert_eq!(l.sizes[0], vec2(1224.0, 1584.0));
        assert_eq!(l.width(), 1224.0 + 2.0 * SIDE_MARGIN);
        // And one that fits leaves the column as wide as the window.
        assert_eq!(ten().width(), WINDOW.x);
    }

    #[test]
    fn a_document_shorter_than_the_window_sits_in_the_middle_of_it() {
        let l = Layout::new(&[LETTER], 0.5, WINDOW);
        // 396 tall in 600: 102 above and below.
        assert_eq!(l.tops[0], 102.0);
        assert_eq!(l.total(), WINDOW.y);
        let rect = l.page_rect(0, Pos2::ZERO);
        assert_eq!(rect.center(), pos2(500.0, 300.0));
    }

    #[test]
    fn fit_page_shows_the_whole_page_and_fit_width_fills_the_window_across() {
        let page = fit_scale(Fit::Page, LETTER, WINDOW);
        let l = Layout::new(&[LETTER], page, WINDOW);
        // Limited by height here, and nothing left to scroll either way.
        assert!(l.sizes[0].y <= WINDOW.y - 2.0 * PAGE_GAP);
        assert!(l.sizes[0].y > WINDOW.y - 2.0 * PAGE_GAP - 2.0);
        assert_eq!((l.total(), l.width()), (WINDOW.y, WINDOW.x));

        let width = fit_scale(Fit::Width, LETTER, WINDOW);
        let l = Layout::new(&[LETTER], width, WINDOW);
        assert!(l.sizes[0].x <= WINDOW.x - 2.0 * SIDE_MARGIN);
        assert!(l.sizes[0].x > WINDOW.x - 2.0 * SIDE_MARGIN - 2.0);
        // No sideways scrolling, but plenty of down.
        assert_eq!(l.width(), WINDOW.x);
        assert!(l.total() > WINDOW.y);
    }

    /// A tall narrow window: fit-page is then limited by width, and is
    /// the same thing as fit-width.
    #[test]
    fn fit_page_is_limited_by_whichever_side_runs_out_first() {
        let tall = vec2(400.0, 2000.0);
        assert_eq!(
            fit_scale(Fit::Page, LETTER, tall),
            fit_scale(Fit::Width, LETTER, tall)
        );
    }

    #[test]
    fn a_fit_is_stable_against_a_window_a_hair_wider() {
        let a = fit_scale(Fit::Width, LETTER, vec2(1002.0, 600.0));
        let b = fit_scale(Fit::Width, LETTER, vec2(1002.3, 600.0));
        assert_eq!(a, b);
    }

    #[test]
    fn a_document_is_fitted_by_its_widest_and_tallest_page() {
        let wide = PageSize {
            width_pt: 792.0,
            height_pt: 612.0,
        };
        let bound = bounding_size(&[LETTER, wide, LETTER]);
        assert_eq!((bound.width_pt, bound.height_pt), (792.0, 792.0));
        assert_eq!(bounding_size(&[]).width_pt, FALLBACK_PAGE_SIZE.width_pt);
    }

    /// Mixed sizes are the reason rows are not a multiplication: a
    /// landscape slide among portrait pages has its own height.
    #[test]
    fn each_page_keeps_its_own_size() {
        let wide = PageSize {
            width_pt: 792.0,
            height_pt: 612.0,
        };
        let l = Layout::new(&[LETTER, wide, LETTER], 1.0, WINDOW);
        assert_eq!(l.tops[2] - l.tops[1], 612.0 + PAGE_GAP);
    }

    #[test]
    fn an_unusable_page_size_still_gets_a_row() {
        let broken = PageSize {
            width_pt: 0.0,
            height_pt: f64::NAN,
        };
        let l = Layout::new(&[broken, LETTER], 1.0, WINDOW);
        assert_eq!(l.sizes[0], vec2(612.0, 792.0));
        assert!(l.total().is_finite());
    }

    #[test]
    fn the_gap_under_a_page_belongs_to_that_page() {
        let l = ten();
        assert_eq!(l.page_at(0.0), 0);
        assert_eq!(l.page_at(12.0 + 792.0 + 5.0), 0);
        assert_eq!(l.page_at(12.0 + 804.0), 1);
        assert_eq!(l.page_at(1.0e9), 9);
    }

    #[test]
    fn visible_is_exactly_the_pages_the_viewport_touches() {
        let l = ten();
        assert_eq!(l.visible(0.0, 600.0), 0..1);
        // Bottom edge reaches into page 1.
        assert_eq!(l.visible(300.0, 900.0), 0..2);
        // Top edge sits in the gap under page 0, so page 0 is gone.
        assert_eq!(l.visible(808.0, 1400.0), 1..2);
        // Far past the end: nothing, rather than a panic.
        assert_eq!(l.visible(1.0e9, 2.0e9), 10..10);
        assert_eq!(Layout::new(&[], 1.0, WINDOW).visible(0.0, 600.0), 0..0);
    }

    #[test]
    fn the_counter_follows_the_page_that_fills_most_of_the_window() {
        let l = ten();
        assert_eq!(l.most_visible(0.0, 600.0), 0);
        // 100 of page 0 left, 488 of page 1 showing.
        assert_eq!(l.most_visible(704.0, 1304.0), 1);
        // Zoomed out so several fit: the first whole one, which is the
        // one a page turn just put at the top.
        let small = Layout::new(&[LETTER; 10], 0.2, WINDOW);
        let top = top_offset(&small, 3);
        assert_eq!(small.most_visible(top, top + 600.0), 3);
    }

    /// The property the anchor exists for.
    #[test]
    fn a_point_on_a_page_survives_a_zoom_and_a_resize() {
        let before = ten();
        let anchor = Anchor {
            page: 4,
            frac: 0.25,
            xfrac: 0.8,
        };
        let found = before.anchor_at(before.point_of(anchor));
        assert_eq!(found.page, 4);
        assert!((found.frac - 0.25).abs() < 1e-4);
        assert!((found.xfrac - 0.8).abs() < 1e-4);

        // Zoomed in, in a window now narrower than the page.
        let after = Layout::new(&[LETTER; 10], 1.5, vec2(700.0, 500.0));
        let moved = after.point_of(found);
        let back = after.anchor_at(moved);
        assert_eq!(back.page, 4);
        assert!((back.frac - 0.25).abs() < 1e-4);
        assert!((back.xfrac - 0.8).abs() < 1e-4);
        // A quarter of the way down page 4 and four fifths across it,
        // in the new geometry.
        let rect = after.page_rect(4, Pos2::ZERO);
        assert!((moved.y - (rect.min.y + 0.25 * rect.height())).abs() < 0.01);
        assert!((moved.x - (rect.min.x + 0.8 * rect.width())).abs() < 0.01);
    }

    #[test]
    fn a_page_turn_lands_exactly_on_a_page_top_and_stays_there_through_a_zoom() {
        let l = ten();
        let at = l.point_of(Anchor::top_of(6));
        assert_eq!(at.y, l.tops[6]);
        assert_eq!(l.anchor_at(at), Anchor::top_of(6));
        let zoomed = Layout::new(&[LETTER; 10], 1.2, vec2(2000.0, 600.0));
        assert_eq!(zoomed.point_of(l.anchor_at(at)).y, zoomed.tops[6]);
    }

    #[test]
    fn next_goes_to_the_following_page_top() {
        let l = ten();
        assert_eq!(l.next_page_offset(0.0), l.tops[1] - PAGE_GAP);
        // From part-way down a page, still the next page's top.
        assert_eq!(l.next_page_offset(400.0), l.tops[1] - PAGE_GAP);
        // Repeating it walks the document one page at a time.
        let mut at = 0.0;
        for _ in 0..3 {
            at = l.next_page_offset(at);
        }
        assert_eq!(at, l.tops[3] - PAGE_GAP);
        // Nothing after the last page.
        let last = l.tops[9] - PAGE_GAP;
        assert_eq!(l.next_page_offset(last), last);
    }

    #[test]
    fn previous_goes_to_the_top_of_this_page_first_then_the_one_before() {
        let l = ten();
        let top_of_3 = l.tops[3] - PAGE_GAP;
        assert_eq!(l.prev_page_offset(top_of_3 + 300.0), top_of_3);
        assert_eq!(l.prev_page_offset(top_of_3), l.tops[2] - PAGE_GAP);
        assert_eq!(l.prev_page_offset(0.0), 0.0);
        assert_eq!(l.prev_page_offset(5.0), 0.0);
    }

    #[test]
    fn what_is_on_screen_renders_first_then_the_way_the_reader_is_going() {
        assert_eq!(render_order(4..6, 10, 1, true), vec![4, 5, 6, 3]);
        assert_eq!(render_order(4..6, 10, 1, false), vec![5, 4, 3, 6]);
        assert_eq!(render_order(4..6, 10, 2, true), vec![4, 5, 6, 7, 3, 2]);
    }

    #[test]
    fn the_render_window_stops_at_the_ends_of_the_document() {
        assert_eq!(render_order(0..2, 10, 1, true), vec![0, 1, 2]);
        assert_eq!(render_order(8..10, 10, 1, true), vec![8, 9, 7]);
        assert_eq!(render_order(0..1, 1, 1, false), vec![0]);
        assert_eq!(render_order(0..0, 0, 1, true), Vec::<usize>::new());
    }

    // ---- the view itself, run without a window ----

    const SCREEN: egui::Vec2 = vec2(800.0, 600.0);

    /// Runs one frame and returns what it showed, plus the rectangle
    /// each painted page was given.
    fn frame(
        ctx: &egui::Context,
        view: &mut View,
        pages: &[PageSize],
        scale: f64,
    ) -> (Shown, Vec<(usize, Rect)>) {
        frame_with(ctx, view, pages, scale, 0.0, Vec::new(), None)
    }

    /// One wheel notch, in points.
    const STEP: f32 = 30.0;

    /// The same, at a given time and with input events.
    fn frame_with(
        ctx: &egui::Context,
        view: &mut View,
        pages: &[PageSize],
        scale: f64,
        time: f64,
        events: Vec<egui::Event>,
        focus: Option<Pos2>,
    ) -> (Shown, Vec<(usize, Rect)>) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
            time: Some(time),
            predicted_dt: 1.0 / 60.0,
            events,
            ..Default::default()
        };
        let mut painted = Vec::new();
        let mut shown = None;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    shown = Some(view.show(ui, pages, scale, focus, STEP, |_, page, rect| {
                        painted.push((page, rect));
                    }));
                });
        });
        (shown.expect("the panel ran"), painted)
    }

    #[test]
    fn a_fresh_view_starts_at_the_top_of_the_first_page() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let (shown, painted) = frame(&ctx, &mut view, &[LETTER; 10], 1.0);
        assert_eq!(shown.current_page, 0);
        assert_eq!(painted[0].0, 0);
        assert_eq!(painted[0].1.min.y, PAGE_GAP);
        // Centred in the window.
        assert_eq!(painted[0].1.center().x, SCREEN.x / 2.0);
    }

    /// The regression test for what was actually broken: pages have to
    /// be placed relative to the scrolled column, not the screen. After
    /// a jump to page 5, page 5 is what is at the top of the window.
    #[test]
    fn a_scrolled_page_is_drawn_where_the_scroll_put_it() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        frame(&ctx, &mut view, &pages, 1.0);
        view.go_to_page(5);
        let (shown, painted) = frame(&ctx, &mut view, &pages, 1.0);
        assert_eq!(shown.current_page, 5);
        assert_eq!(shown.visible, 5..6);
        let (page, rect) = painted[0];
        assert_eq!(page, 5);
        assert!(
            (rect.min.y - PAGE_GAP).abs() < 0.01,
            "top at {}",
            rect.min.y
        );
        // And nothing is painted that is not on screen.
        for (_, rect) in &painted {
            assert!(rect.max.y > 0.0 && rect.min.y < SCREEN.y);
        }
    }

    #[test]
    fn page_turns_walk_forward_and_back() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        frame(&ctx, &mut view, &pages, 1.0);
        view.turn_pages(1);
        view.turn_pages(1);
        let (shown, _) = frame(&ctx, &mut view, &pages, 1.0);
        assert_eq!(shown.current_page, 2);
        assert!(shown.forward);
        view.turn_pages(-1);
        let (shown, painted) = frame(&ctx, &mut view, &pages, 1.0);
        assert_eq!(shown.current_page, 1);
        assert!(!shown.forward);
        assert!((painted[0].1.min.y - PAGE_GAP).abs() < 0.01);
    }

    #[test]
    fn zooming_keeps_the_same_page_at_the_top() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        frame(&ctx, &mut view, &pages, 1.0);
        view.go_to_page(7);
        frame(&ctx, &mut view, &pages, 1.0);
        let (shown, painted) = frame(&ctx, &mut view, &pages, 0.5);
        assert_eq!(painted[0].0, 7);
        assert!((painted[0].1.min.y - PAGE_GAP).abs() < 0.01);
        assert_eq!(shown.current_page, 7);
    }

    /// The scroll area's state is keyed by id and outlives a document.
    /// A new document must not open wherever the last one was left.
    #[test]
    fn a_new_view_ignores_the_previous_one_s_scroll_position() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        frame(&ctx, &mut view, &pages, 1.0);
        view.go_to_page(8);
        frame(&ctx, &mut view, &pages, 1.0);
        let mut fresh = View::new("test");
        let (shown, _) = frame(&ctx, &mut fresh, &pages, 1.0);
        assert_eq!(shown.current_page, 0);
    }

    fn wheel(lines_down: f32) -> egui::Event {
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: vec2(0.0, -lines_down),
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Runs frames with the pointer over the page until nothing is
    /// moving, and returns where the top page ended up.
    fn run_until_still(
        ctx: &egui::Context,
        view: &mut View,
        pages: &[PageSize],
        time: &mut f64,
        mut events: Vec<egui::Event>,
    ) -> (usize, f32) {
        let mut last = (0, 0.0);
        for _ in 0..600 {
            *time += 1.0 / 60.0;
            let (_, painted) = frame_with(
                ctx,
                view,
                pages,
                1.0,
                *time,
                std::mem::take(&mut events),
                None,
            );
            last = (painted[0].0, painted[0].1.min.y);
            if !view.scroller.is_active() {
                break;
            }
        }
        last
    }

    #[test]
    fn a_wheel_notch_scrolls_exactly_one_step_and_eases_there() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        let mut time = 0.0;
        let hover = egui::Event::PointerMoved(pos2(400.0, 300.0));
        run_until_still(&ctx, &mut view, &pages, &mut time, vec![hover]);

        // One frame in, it has started but not arrived.
        time += 1.0 / 60.0;
        let (_, painted) = frame_with(&ctx, &mut view, &pages, 1.0, time, vec![wheel(1.0)], None);
        let first = PAGE_GAP - painted[0].1.min.y;
        assert!(
            first > 0.0 && first < STEP / 2.0,
            "moved {first} in the first frame"
        );
        let (page, top) = run_until_still(&ctx, &mut view, &pages, &mut time, vec![]);
        assert_eq!(page, 0);
        assert!(
            (PAGE_GAP - top - STEP).abs() < 1.0,
            "ended {} down",
            PAGE_GAP - top
        );
    }

    #[test]
    fn a_touchpad_flick_crosses_pages() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        let mut time = 0.0;
        let hover = egui::Event::PointerMoved(pos2(400.0, 300.0));
        run_until_still(&ctx, &mut view, &pages, &mut time, vec![hover]);
        // Six frames at 80 points each: 480 points under the fingers.
        for _ in 0..6 {
            time += 1.0 / 60.0;
            let event = egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: vec2(0.0, -80.0),
                modifiers: egui::Modifiers::NONE,
            };
            frame_with(&ctx, &mut view, &pages, 1.0, time, vec![event], None);
        }
        let (page, _) = run_until_still(&ctx, &mut view, &pages, &mut time, vec![]);
        assert!(page >= 3, "a flick only reached page {page}");
    }

    #[test]
    fn scrolling_up_at_the_top_leaves_nothing_owed() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        let mut time = 0.0;
        let hover = egui::Event::PointerMoved(pos2(400.0, 300.0));
        run_until_still(&ctx, &mut view, &pages, &mut time, vec![hover]);
        let (page, top) = run_until_still(&ctx, &mut view, &pages, &mut time, vec![wheel(-5.0)]);
        assert_eq!((page, top), (0, PAGE_GAP));
        assert!(!view.scroller.is_active());
    }

    #[test]
    fn the_arrow_key_nudge_glides_a_fifth_of_the_window() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        let mut time = 0.0;
        run_until_still(&ctx, &mut view, &pages, &mut time, vec![]);
        view.scroll_by_screens(0.2);
        let (_, top) = run_until_still(&ctx, &mut view, &pages, &mut time, vec![]);
        assert!((PAGE_GAP - top - 0.2 * SCREEN.y).abs() < 1.0);
    }

    /// Ten notches are ten steps: what makes the wheel something you
    /// can land on a line with.
    #[test]
    fn ten_quick_notches_scroll_exactly_ten_steps() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        let mut time = 0.0;
        let hover = egui::Event::PointerMoved(pos2(400.0, 300.0));
        run_until_still(&ctx, &mut view, &pages, &mut time, vec![hover]);
        for _ in 0..10 {
            time += 1.0 / 60.0;
            frame_with(&ctx, &mut view, &pages, 1.0, time, vec![wheel(1.0)], None);
        }
        let (_, top) = run_until_still(&ctx, &mut view, &pages, &mut time, vec![]);
        assert!(
            (PAGE_GAP - top - 10.0 * STEP).abs() < 1.0,
            "ended {} down",
            PAGE_GAP - top
        );
    }

    /// What a pinch needs: the point under the pointer is the same
    /// point of the same page after the zoom as before it.
    #[test]
    fn zooming_about_a_point_keeps_that_point_still() {
        let ctx = egui::Context::default();
        // As the app has them: drawn over the page, not beside it. A
        // scrollbar that takes room of its own makes the window a bar
        // narrower the moment there is something to scroll sideways.
        ctx.style_mut(|s| s.spacing.scroll.floating = true);
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        frame(&ctx, &mut view, &pages, 1.0);
        view.go_to_page(4);
        let (_, before) = frame(&ctx, &mut view, &pages, 1.0);
        let pointer = pos2(300.0, 400.0);
        let (page, rect) = before[0];
        let was = (pointer - rect.min) / rect.size();

        let (_, after) = frame_with(&ctx, &mut view, &pages, 2.0, 0.0, vec![], Some(pointer));
        let (_, rect) = *after.iter().find(|(p, _)| *p == page).expect("still shown");
        let now = (pointer - rect.min) / rect.size();
        assert!((was - now).length() < 1e-3, "{was:?} became {now:?}");
        // The page is now 1224 wide in an 800 window, so it has had to
        // scroll sideways to do that.
        assert!(rect.min.x < 0.0);
    }

    /// Without a pointer to hold on to, the top of the window stays
    /// where it is and the page stays centred.
    #[test]
    fn a_page_zoomed_wider_than_the_window_starts_out_centred() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        frame(&ctx, &mut view, &pages, 1.0);
        let (_, painted) = frame(&ctx, &mut view, &pages, 2.0);
        assert!((painted[0].1.center().x - SCREEN.x / 2.0).abs() < 0.01);
        assert!((painted[0].1.min.y - PAGE_GAP).abs() < 0.01);
    }

    /// Fit page means one page on screen. Not one page and the top
    /// edge of the next.
    #[test]
    fn a_fitted_page_has_the_window_to_itself() {
        let ctx = egui::Context::default();
        ctx.style_mut(|s| s.spacing.scroll.floating = true);
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        // A window size at which the rounded-down fit leaves most of a
        // point of the next page inside the window.
        for height in [600.0_f32, 601.0, 733.0, 1187.0] {
            // Wide, so that it is the height the page is fitted to.
            let screen = vec2(1200.0, height);
            let scale = fit_scale(Fit::Page, LETTER, screen);
            view.go_to_page(4);
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)),
                ..Default::default()
            };
            let mut painted = Vec::new();
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::none())
                    .show(ctx, |ui| {
                        view.show(ui, &pages, scale, None, STEP, |_, page, _| {
                            painted.push(page);
                        });
                    });
            });
            assert_eq!(painted, vec![4], "in a window {height} tall");
        }
    }

    /// The paged view is this view over a one-page slice: a different
    /// slice is a different page, and starts at its top.
    #[test]
    fn a_one_page_view_reports_whether_there_is_anything_to_scroll() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 10];
        let fitted = fit_scale(Fit::Page, LETTER, SCREEN);
        let (shown, painted) = frame(&ctx, &mut view, &pages[3..4], fitted);
        assert!(!shown.overflows);
        assert!((painted[0].1.center() - (SCREEN / 2.0).to_pos2()).length() < 1.0);
        let wide = fit_scale(Fit::Width, LETTER, SCREEN);
        let (shown, _) = frame(&ctx, &mut view, &pages[3..4], wide);
        assert!(shown.overflows);
    }

    #[test]
    fn the_end_of_the_document_is_as_far_as_it_scrolls() {
        let ctx = egui::Context::default();
        let mut view = View::new("test");
        let pages = [LETTER; 3];
        frame(&ctx, &mut view, &pages, 1.0);
        for _ in 0..10 {
            view.turn_pages(1);
        }
        let (shown, painted) = frame(&ctx, &mut view, &pages, 1.0);
        assert_eq!(shown.current_page, 2);
        // The last page's top, not past it: page 2 is taller than the
        // window, so its top is still reachable.
        assert!((painted[0].1.min.y - PAGE_GAP).abs() < 0.01);
    }
}
