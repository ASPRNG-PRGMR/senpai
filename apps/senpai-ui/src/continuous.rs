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
//! The reading position is kept as an [`Anchor`] -- a page and a fraction
//! of the way down it -- rather than as a pixel offset. An offset means
//! something only for one zoom level and one window width; an anchor
//! survives both, which is what lets a zoom or a resize leave the same
//! line of the same page at the top of the window.

use eframe::egui::{self, pos2, vec2, Pos2, Rect};
use senpai_core::model::PageSize;
use std::ops::Range;

/// Space between pages, and above the first and below the last.
pub const PAGE_GAP: f32 = 12.0;
/// Space kept clear on each side of a page that has been fitted to the
/// window.
const SIDE_MARGIN: f32 = 12.0;
/// US Letter, standing in for a page whose own size is unusable (zero,
/// negative or not a number). The row has to have *some* height, and a
/// wrong one is better than a division by zero.
pub const FALLBACK_PAGE_SIZE: PageSize = PageSize {
    width_pt: 612.0,
    height_pt: 792.0,
};

/// A reading position that does not depend on zoom or window width.
///
/// `frac` is measured from one gap above the page's top edge, in page
/// heights, so 0.0 is exactly where a page turn lands: the page's top
/// with its margin showing above it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub page: usize,
    pub frac: f32,
}

impl Anchor {
    pub fn top_of(page: usize) -> Self {
        Self { page, frac: 0.0 }
    }
}

/// Where every page sits, for one zoom level and one window width.
///
/// Coordinates are in points from the top of the column. A page wider
/// than the window is shrunk to fit it, the same rule the paged view
/// applies, so there is never anything to scroll sideways for.
pub struct Layout {
    tops: Vec<f32>,
    sizes: Vec<egui::Vec2>,
    shrunk: Vec<bool>,
    width: f32,
    total: f32,
}

impl Layout {
    pub fn new(pages: &[PageSize], scale: f64, width: f32) -> Self {
        let max_w = (width - 2.0 * SIDE_MARGIN).max(1.0);
        let mut tops = Vec::with_capacity(pages.len());
        let mut sizes = Vec::with_capacity(pages.len());
        let mut shrunk = Vec::with_capacity(pages.len());
        let mut y = PAGE_GAP;
        for page in pages {
            let usable = |p: &PageSize| {
                let (w, h) = ((p.width_pt * scale) as f32, (p.height_pt * scale) as f32);
                (w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0).then_some((w, h))
            };
            let (mut w, mut h) = usable(page)
                .or_else(|| usable(&FALLBACK_PAGE_SIZE))
                .unwrap_or((max_w, max_w));
            let fit = w > max_w;
            if fit {
                h *= max_w / w;
                w = max_w;
            }
            tops.push(y);
            sizes.push(vec2(w, h));
            shrunk.push(fit);
            y += h + PAGE_GAP;
        }
        Self {
            tops,
            sizes,
            shrunk,
            width,
            total: y,
        }
    }

    /// Height of the whole column, margins included.
    pub fn total(&self) -> f32 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.tops.is_empty()
    }

    /// Whether the page was shrunk to fit the window, and so is drawn
    /// smaller than it was rendered.
    pub fn is_shrunk(&self, page: usize) -> bool {
        self.shrunk[page]
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

    /// The scroll offset that puts `anchor` at the top of the window.
    pub fn offset_of(&self, anchor: Anchor) -> f32 {
        if self.is_empty() {
            return 0.0;
        }
        let page = anchor.page.min(self.tops.len() - 1);
        self.tops[page] - PAGE_GAP + anchor.frac * self.sizes[page].y
    }

    /// The inverse of [`Layout::offset_of`].
    pub fn anchor_at(&self, offset: f32) -> Anchor {
        if self.is_empty() {
            return Anchor::top_of(0);
        }
        let y = offset.max(0.0) + PAGE_GAP;
        let page = self.page_at(y);
        Anchor {
            page,
            frac: (y - self.tops[page]) / self.sizes[page].y,
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
        let size = self.sizes[page];
        Rect::from_min_size(
            pos2(
                origin.x + ((self.width - size.x) / 2.0).max(0.0),
                origin.y + self.tops[page],
            ),
            size,
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
}

/// Identifies a layout: zoom, window width, page count. If none of the
/// three has changed, neither has any page's position.
type LayoutKey = (u64, u32, usize);

/// The scroll area, and the reading position in it.
pub struct View {
    anchor: Anchor,
    offset: f32,
    forward: bool,
    layout: Option<(LayoutKey, Layout)>,
    /// Set when the scroll offset has to be recomputed from `anchor`
    /// instead of the other way round: the layout changed under it, or
    /// something asked to go to a particular page.
    reanchor: bool,
    /// Page turns asked for since the last frame; negative is backwards.
    page_steps: i32,
    /// Finer movement asked for since the last frame, in window heights.
    scroll_screens: f32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            anchor: Anchor::top_of(0),
            offset: 0.0,
            forward: true,
            layout: None,
            reanchor: true,
            page_steps: 0,
            scroll_screens: 0.0,
        }
    }
}

impl View {
    /// Puts the top of `page` at the top of the window on the next
    /// frame.
    pub fn go_to_page(&mut self, page: usize) {
        self.anchor = Anchor::top_of(page);
        self.reanchor = true;
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
    /// screen with the page's index, its rectangle, and whether it has
    /// been shrunk to fit.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        pages: &[PageSize],
        scale: f64,
        mut paint: impl FnMut(&egui::Painter, usize, Rect, bool),
    ) -> Shown {
        let avail = ui.available_size();
        let key: LayoutKey = (scale.to_bits(), avail.x.to_bits(), pages.len());
        if self.layout.as_ref().map(|(k, _)| *k) != Some(key) {
            self.layout = Some((key, Layout::new(pages, scale, avail.x)));
            // Every page has just moved. The offset egui is holding now
            // points at some other part of the document entirely.
            self.reanchor = true;
        }
        let layout = &self.layout.as_ref().expect("set just above").1;
        let max_offset = (layout.total() - avail.y).max(0.0);

        let mut target = self.offset;
        let mut forced = std::mem::take(&mut self.reanchor);
        if forced {
            target = layout.offset_of(self.anchor);
        }
        let steps = std::mem::take(&mut self.page_steps);
        for _ in 0..steps.unsigned_abs() {
            target = if steps > 0 {
                layout.next_page_offset(target)
            } else {
                layout.prev_page_offset(target)
            };
            forced = true;
        }
        let screens = std::mem::take(&mut self.scroll_screens);

        let mut area = egui::ScrollArea::vertical()
            .id_salt("continuous_pages")
            .auto_shrink([false, false]);
        if forced {
            area = area.vertical_scroll_offset(target.clamp(0.0, max_offset));
        }
        let out = area.show_viewport(ui, |ui, viewport| {
            // The column's top-left corner *on screen*. egui has already
            // moved it up by the scroll offset, so adding a page's
            // position in the column to it gives where that page is now.
            // The broken version of this view placed pages at their
            // column position as if it were a screen position, which is
            // right only while nothing has been scrolled.
            let origin = ui.max_rect().min;
            ui.allocate_rect(
                Rect::from_min_size(origin, vec2(avail.x, layout.total())),
                egui::Sense::hover(),
            );
            let visible = layout.visible(viewport.min.y, viewport.max.y);
            for page in visible.clone() {
                paint(
                    ui.painter(),
                    page,
                    layout.page_rect(page, origin),
                    layout.is_shrunk(page),
                );
            }
            if screens != 0.0 {
                ui.scroll_with_delta(vec2(0.0, -screens * viewport.height()));
            }
            visible
        });

        let offset = out.state.offset.y;
        if offset != self.offset {
            self.forward = offset > self.offset;
        }
        self.offset = offset;
        self.anchor = layout.anchor_at(offset);
        Shown {
            visible: out.inner,
            current_page: layout.most_visible(offset, offset + avail.y),
            forward: self.forward,
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

    /// Ten letter pages at 1:1 in a window wide enough not to shrink
    /// them: rows are 792 tall, 804 apart, the first at 12.
    fn ten() -> Layout {
        Layout::new(&[LETTER; 10], 1.0, 1000.0)
    }

    #[test]
    fn pages_stack_with_a_gap_between_and_a_margin_at_each_end() {
        let l = ten();
        assert_eq!(l.tops[0], PAGE_GAP);
        assert_eq!(l.tops[1], PAGE_GAP + 792.0 + PAGE_GAP);
        assert_eq!(l.total(), PAGE_GAP + 10.0 * (792.0 + PAGE_GAP));
    }

    #[test]
    fn a_page_wider_than_the_window_is_shrunk_in_proportion() {
        let l = Layout::new(&[LETTER], 2.0, 636.0);
        // 1224 wide into 612 of room: exactly half, so half the height.
        assert!(l.is_shrunk(0));
        assert_eq!(l.sizes[0], vec2(612.0, 792.0));
        assert!(!ten().is_shrunk(0));
    }

    /// Mixed sizes are the reason rows are not a multiplication: a
    /// landscape slide among portrait pages has its own height.
    #[test]
    fn each_page_keeps_its_own_size() {
        let wide = PageSize {
            width_pt: 792.0,
            height_pt: 612.0,
        };
        let l = Layout::new(&[LETTER, wide, LETTER], 1.0, 1000.0);
        assert_eq!(l.tops[2] - l.tops[1], 612.0 + PAGE_GAP);
    }

    #[test]
    fn an_unusable_page_size_still_gets_a_row() {
        let broken = PageSize {
            width_pt: 0.0,
            height_pt: f64::NAN,
        };
        let l = Layout::new(&[broken, LETTER], 1.0, 1000.0);
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
        assert_eq!(Layout::new(&[], 1.0, 1000.0).visible(0.0, 600.0), 0..0);
    }

    #[test]
    fn the_counter_follows_the_page_that_fills_most_of_the_window() {
        let l = ten();
        assert_eq!(l.most_visible(0.0, 600.0), 0);
        // 100 of page 0 left, 488 of page 1 showing.
        assert_eq!(l.most_visible(704.0, 1304.0), 1);
        // Zoomed out so several fit: the first whole one, which is the
        // one a page turn just put at the top.
        let small = Layout::new(&[LETTER; 10], 0.2, 1000.0);
        let top = small.offset_of(Anchor::top_of(3));
        assert_eq!(small.most_visible(top, top + 600.0), 3);
    }

    /// The property the anchor exists for.
    #[test]
    fn a_position_survives_a_zoom_and_a_resize() {
        let before = ten();
        let offset = before.offset_of(Anchor {
            page: 4,
            frac: 0.25,
        });
        let anchor = before.anchor_at(offset);
        assert_eq!(anchor.page, 4);
        assert!((anchor.frac - 0.25).abs() < 1e-4);

        // Zoomed in, in a narrower window that now shrinks the page.
        let after = Layout::new(&[LETTER; 10], 1.5, 700.0);
        let moved = after.offset_of(anchor);
        let back = after.anchor_at(moved);
        assert_eq!(back.page, 4);
        assert!((back.frac - 0.25).abs() < 1e-4);
        // A quarter of the way down page 4, in the new geometry.
        assert!((moved - (after.tops[4] - PAGE_GAP + 0.25 * after.sizes[4].y)).abs() < 0.01);
    }

    #[test]
    fn a_page_turn_lands_exactly_on_a_page_top_and_stays_there_through_a_zoom() {
        let l = ten();
        let at = l.offset_of(Anchor::top_of(6));
        assert_eq!(at, l.tops[6] - PAGE_GAP);
        assert_eq!(l.anchor_at(at), Anchor::top_of(6));
        let zoomed = Layout::new(&[LETTER; 10], 1.2, 2000.0);
        assert_eq!(zoomed.offset_of(l.anchor_at(at)), zoomed.tops[6] - PAGE_GAP);
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
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
            ..Default::default()
        };
        let mut painted = Vec::new();
        let mut shown = None;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    shown = Some(view.show(ui, pages, scale, |_, page, rect, _| {
                        painted.push((page, rect));
                    }));
                });
        });
        (shown.expect("the panel ran"), painted)
    }

    #[test]
    fn a_fresh_view_starts_at_the_top_of_the_first_page() {
        let ctx = egui::Context::default();
        let mut view = View::default();
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
        let mut view = View::default();
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
        let mut view = View::default();
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
        let mut view = View::default();
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
        let mut view = View::default();
        let pages = [LETTER; 10];
        frame(&ctx, &mut view, &pages, 1.0);
        view.go_to_page(8);
        frame(&ctx, &mut view, &pages, 1.0);
        let mut fresh = View::default();
        let (shown, _) = frame(&ctx, &mut fresh, &pages, 1.0);
        assert_eq!(shown.current_page, 0);
    }

    #[test]
    fn the_end_of_the_document_is_as_far_as_it_scrolls() {
        let ctx = egui::Context::default();
        let mut view = View::default();
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
