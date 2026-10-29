//! CommonMark reading support.
//!
//! # Why this does not go through `PdfEngine`
//!
//! Everything else in the reader is a rasterised page: the engine hands
//! back RGBA, the worker renders it off-thread, the LRU caches textures
//! and the view blits one. Markdown deliberately does not join that
//! pipeline, for a reason that is about the format rather than about
//! convenience: a Markdown file has no pages and no fixed measure. Its
//! line breaks belong to the window it is being read in, so the moment
//! it is frozen into a bitmap at some chosen width, resizing and zooming
//! stop meaning what they mean everywhere else, and text selection and
//! links are gone.
//!
//! So Markdown is laid out live, with egui's text stack, into a measured
//! reading column. Zoom changes the type size and the text reflows,
//! which is what a reader expects of this format. The cost is that the
//! render cache and the prefetch machinery do nothing here -- there is
//! no page to cache -- and the page counter has nothing to count.
//!
//! # What the parse keeps
//!
//! Block nesting is flattened: rather than a tree, every block carries
//! how deep in lists and block quotes it sits. Reading only ever walks
//! this list in order, and indentation is the only thing the depth is
//! used for, so a tree would be shape without purpose -- and it makes
//! the parser a flat state machine instead of a recursive one.

use eframe::egui;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::theme;

/// A run of text sharing one set of inline styles.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub strong: bool,
    pub emphasis: bool,
    pub code: bool,
    pub strike: bool,
    /// Destination of the link this run sits inside, if any.
    pub link: Option<String>,
}

impl Span {
    /// Whether two runs can be concatenated instead of kept apart. The
    /// parser emits a separate event per text fragment, so without this
    /// a plain sentence can arrive as a dozen identical-styled runs.
    fn mergeable_with(&self, other: &Span) -> bool {
        self.strong == other.strong
            && self.emphasis == other.emphasis
            && self.code == other.code
            && self.strike == other.strike
            && self.link == other.link
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Bullet,
    Number(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph(Vec<Span>),
    ListItem {
        marker: Marker,
        spans: Vec<Span>,
    },
    Code {
        lang: Option<String>,
        text: String,
    },
    Table {
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Rule,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub kind: BlockKind,
    /// How many lists deep. 0 for a top-level block.
    pub indent: u8,
    /// How many block quotes deep. 0 for a block outside any quote.
    pub quote: u8,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    /// The first level-1 heading, if the file opens with one. Used as the
    /// document's name in the chrome, the way a PDF's title would be.
    pub title: Option<String>,
    pub blocks: Vec<Block>,
}

/// Parses CommonMark, plus the three extensions that are near-universal
/// in real documents: tables, strikethrough and task lists.
///
/// Raw HTML is dropped rather than shown as source. A reader that prints
/// `<div class="x">` at the user is not reading the document to them, and
/// rendering it properly would mean embedding a second engine.
pub fn parse(src: &str) -> Document {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);

    let mut b = Builder::default();
    for event in Parser::new_ext(src, options) {
        b.event(event);
    }
    b.finish()
}

/// What the inline accumulator currently belongs to.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Paragraph,
    Heading(u8),
    Item(Marker),
    Cell,
}

#[derive(Debug, Clone, Copy)]
struct ListLevel {
    ordered: bool,
    next: u64,
}

#[derive(Debug, Default)]
struct TableBuild {
    in_head: bool,
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    spans: Vec<Span>,
    target: Option<Target>,
    style: Span,
    lists: Vec<ListLevel>,
    quote: u8,
    /// Set between `Start(Item)` and the item's first paragraph, so a
    /// loose list -- where the item's text arrives wrapped in a paragraph
    /// -- does not emit an empty item followed by a stray paragraph.
    item_fresh: bool,
    code: Option<(Option<String>, String)>,
    table: Option<TableBuild>,
    /// Depth of nested images, so alt text is collected rather than shown
    /// as if it were body text.
    image: u8,
    image_alt: String,
}

impl Builder {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if self.image > 0 {
                    self.image_alt.push_str(&t);
                } else if let Some((_, text)) = &mut self.code {
                    text.push_str(&t);
                } else {
                    self.push_text(&t, false);
                }
            }
            Event::Code(t) => self.push_text(&t, true),
            // Both break kinds become a space. In a reflowing column a
            // hard break cannot be honoured without a line-break concept
            // in the span model, and the places it matters -- addresses,
            // verse -- are rare next to the cost of carrying one.
            Event::SoftBreak | Event::HardBreak => self.push_text(" ", false),
            Event::Rule => {
                self.flush();
                self.block(BlockKind::Rule);
            }
            Event::TaskListMarker(done) => {
                // ASCII on purpose: the ballot-box glyphs are not in the
                // bundled fonts and would come out as tofu.
                self.push_text(if done { "[x] " } else { "[ ] " }, false)
            }
            Event::FootnoteReference(name) => self.push_text(&format!("[^{name}]"), false),
            // Raw HTML, maths and anything else the parser knows about
            // are deliberately dropped.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.item_fresh {
                    // Keep the item as the target: this paragraph *is*
                    // the item's text.
                    self.item_fresh = false;
                } else if self.table.is_none() {
                    self.target = Some(Target::Paragraph);
                }
            }
            Tag::Heading { level, .. } => {
                self.flush();
                self.target = Some(Target::Heading(level as u8));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let lang = info.split_whitespace().next().unwrap_or("").to_owned();
                        (!lang.is_empty()).then_some(lang)
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                // Load-bearing: a nested list opens while its parent
                // item's own text is still in the accumulator. Without
                // this the parent's text is swallowed into the child,
                // and a three-deep list collapses to one item.
                self.flush();
                self.lists.push(ListLevel {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                });
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(level) if level.ordered => {
                        let n = level.next;
                        level.next += 1;
                        Marker::Number(n)
                    }
                    _ => Marker::Bullet,
                };
                self.target = Some(Target::Item(marker));
                self.item_fresh = true;
            }
            Tag::Table(_) => {
                self.flush();
                self.table = Some(TableBuild::default());
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                }
            }
            Tag::TableCell => self.target = Some(Target::Cell),
            Tag::Emphasis => self.style.emphasis = true,
            Tag::Strong => self.style.strong = true,
            Tag::Strikethrough => self.style.strike = true,
            Tag::Link { dest_url, .. } => self.style.link = Some(dest_url.to_string()),
            Tag::Image { .. } => self.image = self.image.saturating_add(1),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.flush(),
            TagEnd::BlockQuote(_) => self.quote = self.quote.saturating_sub(1),
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    let text = text.strip_suffix('\n').unwrap_or(&text).to_owned();
                    self.block(BlockKind::Code { lang, text });
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            // Covers a tight list, whose item text arrives with no
            // paragraph around it at all.
            TagEnd::Item => {
                self.flush();
                self.item_fresh = false;
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.spans);
                self.target = None;
                if let Some(t) = &mut self.table {
                    t.row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    if t.in_head {
                        t.head = row;
                        t.in_head = false;
                    } else {
                        t.rows.push(row);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.block(BlockKind::Table {
                        head: t.head,
                        rows: t.rows,
                    });
                }
            }
            TagEnd::Emphasis => self.style.emphasis = false,
            TagEnd::Strong => self.style.strong = false,
            TagEnd::Strikethrough => self.style.strike = false,
            TagEnd::Link => self.style.link = None,
            TagEnd::Image => {
                self.image = self.image.saturating_sub(1);
                if self.image == 0 {
                    let alt = std::mem::take(&mut self.image_alt);
                    let label = if alt.is_empty() {
                        "[image]".to_owned()
                    } else {
                        format!("[image: {alt}]")
                    };
                    // Marked as code so it reads as a placeholder rather
                    // than as part of the sentence around it.
                    let mut span = self.style.clone();
                    span.code = true;
                    span.text = label;
                    self.spans.push(span);
                }
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str, code: bool) {
        if self.target.is_none() {
            // Text outside any block -- a stray fragment between
            // containers. Attach it to a paragraph rather than lose it.
            self.target = Some(Target::Paragraph);
        }
        let mut span = self.style.clone();
        span.code = code || span.code;
        span.text = text.to_owned();
        match self.spans.last_mut() {
            Some(last) if last.mergeable_with(&span) => last.text.push_str(&span.text),
            _ => self.spans.push(span),
        }
    }

    /// Emits whatever the accumulator holds as a block.
    fn flush(&mut self) {
        let Some(target) = self.target.take() else {
            return;
        };
        let spans = std::mem::take(&mut self.spans);
        // An empty paragraph is noise; an empty list item is a real,
        // visible thing (a bullet with nothing after it).
        if spans.is_empty() && !matches!(target, Target::Item(_)) {
            return;
        }
        let kind = match target {
            Target::Paragraph => BlockKind::Paragraph(spans),
            Target::Heading(level) => BlockKind::Heading { level, spans },
            Target::Item(marker) => BlockKind::ListItem { marker, spans },
            // A cell is drained by TagEnd::TableCell and never reaches here.
            Target::Cell => return,
        };
        self.block(kind);
    }

    fn block(&mut self, kind: BlockKind) {
        // A list item's own depth is the list it belongs to, not the one
        // below it: `lists.len()` counts the enclosing list too.
        let indent = match kind {
            BlockKind::ListItem { .. } => self.lists.len().saturating_sub(1),
            _ => self.lists.len(),
        };
        self.blocks.push(Block {
            kind,
            indent: indent.min(u8::MAX as usize) as u8,
            quote: self.quote,
        });
    }

    fn finish(mut self) -> Document {
        self.flush();
        let title = self.blocks.iter().find_map(|b| match &b.kind {
            BlockKind::Heading { level: 1, spans } => Some(plain(spans)),
            _ => None,
        });
        Document {
            title,
            blocks: self.blocks,
        }
    }
}

/// The text of a run of spans with all styling dropped.
pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

// ── Rendering ────────────────────────────────────────────────────────────

/// Unzoomed width of the reading column, in points.
///
/// Text is not set to the window's full width on purpose: a line that
/// runs the width of a maximised window is measurably harder to read,
/// because the eye loses its place on the return sweep. This is roughly
/// 90 characters at the base size, at the wide end of the usual advice
/// and chosen so code blocks and tables are not cramped by it.
const COLUMN_PT: f32 = 760.0;

/// Body text size at zoom 1.0.
const BASE_FONT: f32 = 15.0;

/// Body text sits a clear step below emphasised text rather than at the
/// same brightness, because the bundled font has no bold face: weight
/// has to be carried by contrast or `**strong**` renders identically to
/// the sentence around it.
struct Palette {
    text: egui::Color32,
    dim: egui::Color32,
    faint: egui::Color32,
    link: egui::Color32,
    code_bg: egui::Color32,
    rule: egui::Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            text: egui::Color32::from_gray(242),
            dim: egui::Color32::from_gray(186),
            faint: egui::Color32::from_gray(132),
            link: theme::ACCENT,
            code_bg: egui::Color32::from_gray(46),
            rule: egui::Color32::from_gray(64),
        }
    } else {
        Palette {
            text: egui::Color32::from_gray(12),
            dim: egui::Color32::from_gray(64),
            faint: egui::Color32::from_gray(118),
            link: egui::Color32::from_rgb(28, 100, 190),
            code_bg: egui::Color32::from_gray(222),
            rule: egui::Color32::from_gray(196),
        }
    }
}

/// Lays the document out into `ui`, which is expected to be a scroll
/// area's contents.
///
/// `zoom` multiplies every size, so the reader's existing zoom keys mean
/// "bigger type" here rather than "more pixels per page" -- the same
/// gesture, the right behaviour for a reflowing format.
pub fn show(ui: &mut egui::Ui, doc: &Document, zoom: f32) {
    let pal = palette(ui.visuals().dark_mode);
    let base = BASE_FONT * zoom;
    // `space_before` is the only thing that decides vertical rhythm.
    // Leaving egui's default row spacing on top of it pushed list items
    // as far apart as paragraphs, so a list stopped reading as one
    // object.
    ui.spacing_mut().item_spacing.y = 0.0;

    for (i, block) in doc.blocks.iter().enumerate() {
        ui.add_space(space_before(block, i == 0) * zoom);

        let indent = (block.indent as f32 * 20.0 + block.quote as f32 * 16.0) * zoom;
        let laid_out = ui.horizontal_top(|ui| {
            if indent > 0.0 {
                ui.add_space(indent);
            }
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                body(ui, block, base, zoom, &pal);
            });
        });

        // Painted after the fact because the bar has to span the block's
        // full height, which is only known once it has been laid out.
        for depth in 0..block.quote {
            let x = laid_out.response.rect.left() + (depth as f32 * 16.0 + 3.0) * zoom;
            ui.painter().vline(
                x,
                laid_out.response.rect.y_range(),
                egui::Stroke::new(2.0_f32 * zoom, pal.faint.gamma_multiply(0.7)),
            );
        }
    }
}

fn body(ui: &mut egui::Ui, block: &Block, base: f32, zoom: f32, pal: &Palette) {
    match &block.kind {
        BlockKind::Heading { level, spans } => {
            inline(ui, spans, base * heading_scale(*level), true, true, pal);
            // A rule under the top two levels, the way a chapter opening
            // is set: it separates sections without needing extra space.
            if *level <= 2 {
                ui.add_space(5.0 * zoom);
                rule(ui, pal);
            }
        }
        BlockKind::Paragraph(spans) => inline(ui, spans, base, false, true, pal),
        BlockKind::ListItem { marker, spans } => {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0 * zoom;
                let label = match marker {
                    Marker::Bullet => "\u{2022}".to_owned(),
                    Marker::Number(n) => format!("{n}."),
                };
                // Painted into an exactly-sized box rather than added as
                // a widget: a label gets centred in whatever box it is
                // given, which left bullets floating above the text they
                // belong to. Anchoring to the box's top edge at the body
                // size puts the marker on the first line's baseline, and
                // right-aligning keeps multi-digit numbers lined up.
                let (slot, _) = ui
                    .allocate_exact_size(egui::vec2(18.0 * zoom, base * 1.3), egui::Sense::hover());
                ui.painter().text(
                    slot.right_top(),
                    egui::Align2::RIGHT_TOP,
                    label,
                    egui::FontId::proportional(base),
                    pal.faint,
                );
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    inline(ui, spans, base, false, true, pal);
                });
            });
        }
        BlockKind::Code { lang, text } => {
            egui::Frame::none()
                .fill(pal.code_bg)
                .rounding(egui::Rounding::same(10.0 * zoom))
                .inner_margin(egui::Margin::symmetric(12.0 * zoom, 10.0 * zoom))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if let Some(lang) = lang {
                        ui.label(egui::RichText::new(lang).size(base * 0.72).color(pal.faint));
                    }
                    ui.label(
                        egui::RichText::new(text)
                            .monospace()
                            .size(base * 0.88)
                            .color(pal.text),
                    );
                });
        }
        BlockKind::Table { head, rows } => {
            egui::Grid::new(ui.next_auto_id())
                .striped(true)
                .spacing(egui::vec2(16.0 * zoom, 7.0 * zoom))
                .show(ui, |ui| {
                    if !head.is_empty() {
                        for cell in head {
                            // Cells do not wrap: a grid hands each column
                            // the width it asks for, so wrapping inside
                            // one makes column widths depend on their own
                            // contents' wrapping. Short cells are the
                            // overwhelmingly common case.
                            inline(ui, cell, base * 0.95, true, false, pal);
                        }
                        ui.end_row();
                    }
                    for row in rows {
                        for cell in row {
                            inline(ui, cell, base * 0.95, false, false, pal);
                        }
                        ui.end_row();
                    }
                });
        }
        BlockKind::Rule => rule(ui, pal),
    }
}

fn inline(ui: &mut egui::Ui, spans: &[Span], size: f32, bold_all: bool, wrap: bool, pal: &Palette) {
    let draw = |ui: &mut egui::Ui| {
        // Runs are butted together: the gaps between words already live
        // in the text, and egui's default item spacing would double them.
        ui.spacing_mut().item_spacing.x = 0.0;
        for span in spans {
            let mut rich = egui::RichText::new(&span.text);
            if span.code {
                rich = rich
                    .monospace()
                    .size(size * 0.92)
                    .background_color(pal.code_bg);
            } else {
                rich = rich.size(size);
            }
            if span.strong || bold_all {
                rich = rich.strong();
            }
            if span.emphasis {
                rich = rich.italics();
            }
            if span.strike {
                rich = rich.strikethrough();
            }
            // Colour last: it has to win over whatever `strong()` set.
            match &span.link {
                Some(url) => {
                    if ui.add(egui::Link::new(rich.color(pal.link))).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    }
                }
                None => {
                    // There is no bold face in the bundled font, so
                    // `strong()` alone changes nothing visible. Weight is
                    // carried by brightness instead: emphasised text sits
                    // at the full text colour, body text a step below it.
                    let colour = if bold_all || span.strong {
                        pal.text
                    } else {
                        pal.dim
                    };
                    ui.label(rich.color(colour));
                }
            }
        }
    };
    if wrap {
        ui.horizontal_wrapped(draw);
    } else {
        ui.horizontal(draw);
    }
}

fn rule(ui: &mut egui::Ui, pal: &Palette) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0_f32, pal.rule),
    );
}

fn heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.95,
        2 => 1.55,
        3 => 1.28,
        4 => 1.12,
        5 => 1.0,
        _ => 0.92,
    }
}

/// Leading above a block. Headings get more, so sections read as
/// sections; list items get almost none, so a list reads as one object.
fn space_before(block: &Block, first: bool) -> f32 {
    if first {
        return 0.0;
    }
    match &block.kind {
        BlockKind::Heading { level, .. } => {
            if *level <= 2 {
                24.0
            } else {
                17.0
            }
        }
        BlockKind::ListItem { .. } => 5.0,
        BlockKind::Code { .. } | BlockKind::Table { .. } => 13.0,
        BlockKind::Rule => 15.0,
        BlockKind::Paragraph(_) => 11.0,
    }
}

/// The width the reading column should take inside `available`.
///
/// Split out so the clamping rule can be checked without a window.
pub fn column_width(available: f32, zoom: f32) -> f32 {
    (COLUMN_PT * zoom).min(available)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<BlockKind> {
        parse(src).blocks.into_iter().map(|b| b.kind).collect()
    }

    fn text_of(src: &str) -> Vec<String> {
        parse(src)
            .blocks
            .iter()
            .map(|b| match &b.kind {
                BlockKind::Heading { spans, .. }
                | BlockKind::Paragraph(spans)
                | BlockKind::ListItem { spans, .. } => plain(spans),
                BlockKind::Code { text, .. } => text.clone(),
                BlockKind::Table { .. } => "<table>".into(),
                BlockKind::Rule => "---".into(),
            })
            .collect()
    }

    #[test]
    fn headings_carry_their_level_and_the_first_h1_titles_the_document() {
        let doc = parse("# Title\n\n## Section\n\n### Deeper\n");
        assert_eq!(doc.title.as_deref(), Some("Title"));
        let levels: Vec<u8> = doc
            .blocks
            .iter()
            .filter_map(|b| match b.kind {
                BlockKind::Heading { level, .. } => Some(level),
                _ => None,
            })
            .collect();
        assert_eq!(levels, vec![1, 2, 3]);
    }

    #[test]
    fn a_document_without_an_h1_has_no_title() {
        assert_eq!(parse("## Only a section\n").title, None);
    }

    /// The parser emits a separate event per text fragment; without
    /// merging, one plain sentence arrives as a pile of identical runs.
    #[test]
    fn adjacent_runs_of_the_same_style_are_merged() {
        let doc = parse("plain text here\n");
        match &doc.blocks[0].kind {
            BlockKind::Paragraph(spans) => assert_eq!(spans.len(), 1),
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn emphasis_and_strong_and_code_are_kept_apart() {
        let doc = parse("a *b* **c** `d`\n");
        let BlockKind::Paragraph(spans) = &doc.blocks[0].kind else {
            panic!("expected a paragraph");
        };
        let styled: Vec<(&str, bool, bool, bool)> = spans
            .iter()
            .map(|s| (s.text.as_str(), s.emphasis, s.strong, s.code))
            .collect();
        assert!(styled.contains(&("b", true, false, false)));
        assert!(styled.contains(&("c", false, true, false)));
        assert!(styled.contains(&("d", false, false, true)));
    }

    #[test]
    fn a_link_puts_its_destination_on_every_span_inside_it() {
        let doc = parse("see [the **docs**](https://example.com/x) now\n");
        let BlockKind::Paragraph(spans) = &doc.blocks[0].kind else {
            panic!("expected a paragraph");
        };
        let linked: Vec<&Span> = spans.iter().filter(|s| s.link.is_some()).collect();
        assert!(!linked.is_empty());
        assert!(linked
            .iter()
            .all(|s| s.link.as_deref() == Some("https://example.com/x")));
        assert_eq!(plain(spans), "see the docs now");
    }

    #[test]
    fn a_fenced_block_keeps_its_language_and_drops_the_trailing_newline() {
        let kinds = kinds("```rust\nfn main() {}\n```\n");
        assert_eq!(
            kinds,
            vec![BlockKind::Code {
                lang: Some("rust".into()),
                text: "fn main() {}".into(),
            }]
        );
    }

    #[test]
    fn an_unfenced_indented_block_is_still_code() {
        let kinds = kinds("    just code\n");
        assert!(matches!(kinds[0], BlockKind::Code { lang: None, .. }));
    }

    /// A tight list gives its item text with no paragraph wrapper at all,
    /// a loose one wraps it. Both must come out as one item each.
    #[test]
    fn tight_and_loose_lists_both_yield_one_item_per_entry() {
        for src in ["- a\n- b\n", "- a\n\n- b\n"] {
            let items: Vec<String> = parse(src)
                .blocks
                .iter()
                .filter_map(|b| match &b.kind {
                    BlockKind::ListItem { spans, .. } => Some(plain(spans)),
                    _ => None,
                })
                .collect();
            assert_eq!(items, vec!["a", "b"], "source: {src:?}");
        }
    }

    #[test]
    fn ordered_lists_number_from_their_start() {
        let markers: Vec<Marker> = parse("3. c\n4. d\n")
            .blocks
            .iter()
            .filter_map(|b| match b.kind {
                BlockKind::ListItem { marker, .. } => Some(marker),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec![Marker::Number(3), Marker::Number(4)]);
    }

    #[test]
    fn nesting_shows_up_as_indent_depth() {
        let doc = parse("- outer\n  - inner\n    - deepest\n");
        let depths: Vec<u8> = doc
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::ListItem { .. }))
            .map(|b| b.indent)
            .collect();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn block_quotes_show_up_as_quote_depth() {
        let doc = parse("> quoted\n\n>> deeper\n\nplain\n");
        let depths: Vec<u8> = doc.blocks.iter().map(|b| b.quote).collect();
        assert_eq!(depths, vec![1, 2, 0]);
    }

    #[test]
    fn tables_split_head_from_body() {
        let doc = parse("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n");
        let BlockKind::Table { head, rows } = &doc.blocks[0].kind else {
            panic!("expected a table, got {:?}", doc.blocks[0].kind);
        };
        assert_eq!(
            head.iter().map(|c| plain(c)).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1].iter().map(|c| plain(c)).collect::<Vec<_>>(),
            ["3", "4"]
        );
    }

    #[test]
    fn a_rule_is_its_own_block() {
        assert_eq!(kinds("a\n\n---\n\nb\n").len(), 3);
        assert!(matches!(kinds("a\n\n---\n\nb\n")[1], BlockKind::Rule));
    }

    #[test]
    fn task_markers_use_ascii_so_they_cannot_come_out_as_tofu() {
        let rendered = text_of("- [x] done\n- [ ] todo\n");
        assert_eq!(rendered, vec!["[x] done", "[ ] todo"]);
    }

    /// Showing the user `<div class="x">` is not reading the document to
    /// them, so raw HTML is dropped rather than printed as source.
    #[test]
    fn raw_html_is_dropped_rather_than_shown() {
        let rendered = text_of("<div class=\"x\">\n\nreal text\n\n</div>\n");
        assert_eq!(rendered, vec!["real text"]);
    }

    #[test]
    fn an_image_becomes_a_named_placeholder() {
        let doc = parse("![a diagram](x.png)\n");
        let BlockKind::Paragraph(spans) = &doc.blocks[0].kind else {
            panic!("expected a paragraph");
        };
        assert_eq!(plain(spans), "[image: a diagram]");
        assert!(spans[0].code, "placeholders are styled as code");
    }

    #[test]
    fn an_image_with_no_alt_text_still_says_there_was_one() {
        let doc = parse("![](x.png)\n");
        let BlockKind::Paragraph(spans) = &doc.blocks[0].kind else {
            panic!("expected a paragraph");
        };
        assert_eq!(plain(spans), "[image]");
    }

    #[test]
    fn an_empty_document_parses_to_nothing_rather_than_failing() {
        assert!(parse("").blocks.is_empty());
        assert!(parse("\n\n   \n").blocks.is_empty());
    }

    #[test]
    fn the_reading_column_is_capped_but_never_exceeds_the_window() {
        // Narrow window: the column gives way.
        assert_eq!(column_width(400.0, 1.0), 400.0);
        // Wide window: the column holds its measure.
        assert_eq!(column_width(2000.0, 1.0), COLUMN_PT);
        // Zoom widens it, since the type grew too.
        assert_eq!(column_width(4000.0, 2.0), COLUMN_PT * 2.0);
    }
}
