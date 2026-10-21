//! Platform-independent document core shared by every renderer backend.
//!
//! This module owns everything that does not touch a drawing or text API: the
//! theme constants, the markdown parse into a flat [`Block`] list, heading
//! anchors, link classification, and the case-fold used by search. Both the
//! Direct2D/DirectWrite backend (`native.rs`, Windows) and the X11 + FreeType
//! + FreeType backend (`x11.rs`, Linux) build their display lists from this.
//!
//! Styling is hardcoded to match the CSS used by the original WebView2 path,
//! so there is no cascade, selector matching, or general box model here — just
//! a fixed vertical stack of blocks in a single centred column.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pulldown_cmark::{Event as MdEvent, HeadingLevel, Options, Parser, Tag, TagEnd};

// ---------------------------------------------------------------------------
// Theme — mirrors the CSS constants in the original WebView2 path.
// ---------------------------------------------------------------------------

pub const BG: u32 = 0x0d1117;
pub const FG: u32 = 0xe6edf3;
pub const FG_STRONG: u32 = 0xf0f6fc;
pub const FG_MUTED: u32 = 0x8b949e;
pub const SURFACE: u32 = 0x161b22;
pub const BORDER: u32 = 0x30363d;
pub const BORDER_SOFT: u32 = 0x21262d;
pub const LINK: u32 = 0x58a6ff;

pub const MAX_COLUMN: f32 = 860.0;
pub const PAD_X: f32 = 24.0;
pub const PAD_Y: f32 = 32.0;

pub const BODY_SIZE: f32 = 16.0;
pub const BODY_LINE: f32 = 25.6;
pub const ITEM_LINE: f32 = 27.2;
pub const CODE_SIZE: f32 = 13.6;
pub const CODE_LINE: f32 = 19.7;

pub const BLOCK_GAP: f32 = 16.0;
pub const HEADING_GAP_ABOVE: f32 = 24.0;
pub const RULE_GAP: f32 = 24.0;
pub const CODE_PAD: f32 = 16.0;
pub const QUOTE_BAR: f32 = 4.0;
pub const QUOTE_PAD: f32 = 16.0;
pub const LIST_INDENT: f32 = 32.0;
pub const ITEM_GAP: f32 = 8.0;
pub const CELL_PAD_X: f32 = 13.0;
pub const CELL_PAD_Y: f32 = 6.0;

pub const THUMB: u32 = 0x30363d;
pub const THUMB_HOVER: u32 = 0x484f58;
pub const SCROLLBAR_W: f32 = 10.0;
pub const THUMB_MIN: f32 = 28.0;

/// Search highlights, drawn translucent under the text (GitHub's find colours).
pub const MATCH: u32 = 0xbb8009;
pub const MATCH_ALPHA: f32 = 0.4;
pub const MATCH_CURRENT_ALPHA: f32 = 0.85;

/// Text selection, translucent under the text (GitHub's selection colour).
pub const SELECTION: u32 = 0x388bfd;
pub const SELECTION_ALPHA: f32 = 0.4;

/// Distance, in DIPs, a press must move before it becomes a drag-select
/// rather than a click (which may open a link).
pub const DRAG_THRESHOLD: f32 = 4.0;

/// Ctrl+F bar, in viewport DIPs, pinned top-right beside the scrollbar.
pub const BAR_W: f32 = 300.0;
pub const BAR_H: f32 = 30.0;
pub const BAR_MARGIN: f32 = 8.0;
pub const BAR_PAD: f32 = 10.0;
pub const BAR_ARROW_W: f32 = 22.0;

/// A logical colour slot. Each backend maps these to its own brush/paint table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Background,
    Text,
    Strong,
    Muted,
    Surface,
    Border,
    BorderSoft,
    Link,
    Thumb,
    ThumbHover,
    Match,
    MatchCurrent,
    Selection,
}

// ---------------------------------------------------------------------------
// Document model
// ---------------------------------------------------------------------------

/// An inline style applied to a UTF-16 range of a block's text.
#[derive(Clone, Copy)]
pub struct Span {
    pub start: u32,
    pub len: u32,
    pub style: Style,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Strong,
    Emphasis,
    Code,
    Link,
}

/// A link destination covering a UTF-16 range of a block's text.
#[derive(Clone)]
pub struct LinkSpan {
    pub start: u32,
    pub len: u32,
    pub dest: String,
}

/// A run of text with its inline styles, stored as UTF-16.
///
/// UTF-16 is the storage unit because DirectWrite indexes text in UTF-16 code
/// units; the FreeType backend uses the same unit so match and caret offsets
/// are identical across backends and the parse stays byte-for-byte shared.
#[derive(Default, Clone)]
pub struct Text {
    pub utf16: Vec<u16>,
    pub spans: Vec<Span>,
    pub links: Vec<LinkSpan>,
}

impl Text {
    pub fn push(&mut self, s: &str) {
        self.utf16.extend(s.encode_utf16());
    }

    pub fn cursor(&self) -> u32 {
        self.utf16.len() as u32
    }

    pub fn style(&mut self, start: u32, style: Style) {
        let len = self.cursor().saturating_sub(start);
        if len > 0 {
            self.spans.push(Span { start, len, style });
        }
    }

    pub fn is_blank(&self) -> bool {
        self.utf16.iter().all(|c| *c == 0x20 || *c == 0x0a || *c == 0x09)
    }
}

/// Which text style a block is rendered with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Heading(u8),
    Body,
    Code,
    Item,
}

#[derive(Clone)]
pub struct TableRow {
    pub cells: Vec<Text>,
    pub header: bool,
}

#[derive(Clone)]
pub enum Block {
    /// A run of text. `indent` is added to the left edge; `quote_depth` draws
    /// that many blockquote bars and mutes the text.
    Text {
        flavor: Flavor,
        text: Text,
        indent: f32,
        quote_depth: u32,
        tight: bool,
    },
    Rule,
    Table(Vec<TableRow>),
}

/// Converts markdown source into the flat block list the layout stage consumes.
///
/// Args:
///     source: Raw markdown text.
///
/// Returns:
///     Blocks in document order, and each heading's anchor slug paired with
///     the index of its block.
pub fn parse(source: &str) -> (Vec<Block>, Vec<(String, usize)>) {
    let parser = Parser::new_ext(source, Options::all());

    let mut blocks: Vec<Block> = Vec::new();
    let mut text = Text::default();
    let mut flavor = Flavor::Body;
    let mut style_starts: Vec<(Style, u32)> = Vec::new();
    let mut link_starts: Vec<(u32, String)> = Vec::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut quote_depth: u32 = 0;
    let mut in_block = false;
    let mut tight = false;

    // Heading anchors: an explicit `{#id}` wins, otherwise a GitHub-style slug.
    let mut anchors: Vec<(String, usize)> = Vec::new();
    let mut slug_counts: HashMap<String, usize> = HashMap::new();
    let mut heading_id: Option<String> = None;

    // Table accumulation
    let mut table_rows: Vec<TableRow> = Vec::new();
    let mut row_cells: Vec<Text> = Vec::new();
    let mut in_table = false;
    let mut in_header = false;
    let mut in_cell = false;

    let flush = |text: &mut Text,
                     blocks: &mut Vec<Block>,
                     flavor: Flavor,
                     quote_depth: u32,
                     indent: f32,
                     tight: bool| {
        if !text.utf16.is_empty() && !(text.is_blank() && flavor != Flavor::Code) {
            blocks.push(Block::Text {
                flavor,
                text: std::mem::take(text),
                indent,
                quote_depth,
                tight,
            });
        } else {
            text.utf16.clear();
            text.spans.clear();
            text.links.clear();
        }
    };

    for event in parser {
        match event {
            MdEvent::Start(Tag::Heading { level, id, .. }) => {
                let n = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                flavor = Flavor::Heading(n);
                in_block = true;
                heading_id = id.map(|id| id.to_string());
            }
            MdEvent::End(TagEnd::Heading(_)) => {
                let index = blocks.len();
                let label = String::from_utf16_lossy(&text.utf16);
                let indent = list_indent(&list_stack);
                flush(&mut text, &mut blocks, flavor, quote_depth, indent, false);
                if blocks.len() > index {
                    let base = heading_id.take().unwrap_or_else(|| slugify(&label));
                    // GitHub disambiguates repeats as `name`, `name-1`, `name-2`.
                    let seen = slug_counts.entry(base.clone()).or_insert(0);
                    let slug = if *seen == 0 { base } else { format!("{base}-{seen}") };
                    *seen += 1;
                    anchors.push((slug, index));
                }
                heading_id = None;
                flavor = Flavor::Body;
                in_block = false;
            }

            MdEvent::Start(Tag::Paragraph) => {
                flavor = if list_stack.is_empty() { Flavor::Body } else { Flavor::Item };
                in_block = true;
            }
            MdEvent::End(TagEnd::Paragraph) => {
                let indent = list_indent(&list_stack);
                flush(&mut text, &mut blocks, flavor, quote_depth, indent, tight);
                flavor = Flavor::Body;
                in_block = false;
                tight = false;
            }

            MdEvent::Start(Tag::CodeBlock(_)) => {
                flavor = Flavor::Code;
                in_block = true;
            }
            MdEvent::End(TagEnd::CodeBlock) => {
                // Trailing newline from the fence would render as a blank line.
                while text.utf16.last() == Some(&0x0a) {
                    text.utf16.pop();
                }
                let indent = list_indent(&list_stack);
                flush(&mut text, &mut blocks, Flavor::Code, quote_depth, indent, false);
                flavor = Flavor::Body;
                in_block = false;
            }

            MdEvent::Start(Tag::BlockQuote(_)) => quote_depth += 1,
            MdEvent::End(TagEnd::BlockQuote(_)) => quote_depth = quote_depth.saturating_sub(1),

            MdEvent::Start(Tag::List(start)) => list_stack.push(start),
            MdEvent::End(TagEnd::List(_)) => {
                list_stack.pop();
            }

            MdEvent::Start(Tag::Item) => {
                // The marker is written into the text itself; the block is then
                // indented so wrapped lines align past it.
                let marker = match list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => String::from("\u{2022} "),
                };
                text.push(&marker);
                flavor = Flavor::Item;
                in_block = true;
                tight = true;
            }
            MdEvent::End(TagEnd::Item) => {
                let indent = list_indent(&list_stack);
                flush(&mut text, &mut blocks, Flavor::Item, quote_depth, indent, true);
                in_block = false;
            }

            MdEvent::Start(Tag::Table(_)) => {
                in_table = true;
                table_rows.clear();
            }
            MdEvent::End(TagEnd::Table) => {
                if !table_rows.is_empty() {
                    blocks.push(Block::Table(std::mem::take(&mut table_rows)));
                }
                in_table = false;
            }
            MdEvent::Start(Tag::TableHead) => {
                in_header = true;
                row_cells.clear();
            }
            MdEvent::End(TagEnd::TableHead) => {
                table_rows.push(TableRow {
                    cells: std::mem::take(&mut row_cells),
                    header: true,
                });
                in_header = false;
            }
            MdEvent::Start(Tag::TableRow) => row_cells.clear(),
            MdEvent::End(TagEnd::TableRow) => {
                table_rows.push(TableRow {
                    cells: std::mem::take(&mut row_cells),
                    header: false,
                });
            }
            MdEvent::Start(Tag::TableCell) => {
                in_cell = true;
                in_block = true;
            }
            MdEvent::End(TagEnd::TableCell) => {
                row_cells.push(std::mem::take(&mut text));
                in_cell = false;
                in_block = false;
            }

            MdEvent::Start(Tag::Strong) => style_starts.push((Style::Strong, text.cursor())),
            MdEvent::End(TagEnd::Strong) => close_style(&mut text, &mut style_starts, Style::Strong),
            MdEvent::Start(Tag::Emphasis) => style_starts.push((Style::Emphasis, text.cursor())),
            MdEvent::End(TagEnd::Emphasis) => {
                close_style(&mut text, &mut style_starts, Style::Emphasis)
            }
            MdEvent::Start(Tag::Link { dest_url, .. }) => {
                style_starts.push((Style::Link, text.cursor()));
                link_starts.push((text.cursor(), dest_url.to_string()));
            }
            MdEvent::End(TagEnd::Link) => {
                close_style(&mut text, &mut style_starts, Style::Link);
                if let Some((start, dest)) = link_starts.pop() {
                    let len = text.cursor().saturating_sub(start);
                    if len > 0 {
                        text.links.push(LinkSpan { start, len, dest });
                    }
                }
            }

            // Images are not decoded; show the alt text instead.
            MdEvent::Start(Tag::Image { .. }) => {
                style_starts.push((Style::Emphasis, text.cursor()));
                text.push("[image: ");
            }
            MdEvent::End(TagEnd::Image) => {
                text.push("]");
                close_style(&mut text, &mut style_starts, Style::Emphasis);
            }

            MdEvent::Text(t) => {
                if in_block || in_cell {
                    text.push(&t);
                }
            }
            MdEvent::Code(t) => {
                let start = text.cursor();
                text.push(&t);
                text.style(start, Style::Code);
            }
            MdEvent::SoftBreak => text.push(" "),
            MdEvent::HardBreak => text.push("\n"),
            MdEvent::Rule => {
                let indent = list_indent(&list_stack);
                flush(&mut text, &mut blocks, flavor, quote_depth, indent, false);
                blocks.push(Block::Rule);
            }

            // Inline and block HTML is passed through as literal text rather
            // than interpreted; a viewer should not execute markup.
            MdEvent::Html(t) | MdEvent::InlineHtml(t) => {
                if in_block || in_cell {
                    let start = text.cursor();
                    text.push(t.trim_end_matches('\n'));
                    text.style(start, Style::Code);
                }
            }
            _ => {}
        }
    }

    let indent = list_indent(&list_stack);
    flush(&mut text, &mut blocks, flavor, quote_depth, indent, false);
    let _ = in_table;
    let _ = in_header;
    (blocks, anchors)
}

/// Converts heading text to a GitHub-style anchor slug.
///
/// Lowercases, keeps letters, digits, `-` and `_`, turns each space into `-`,
/// and drops everything else, matching the anchors GitHub generates.
///
/// Args:
///     label: The heading's visible text.
///
/// Returns:
///     The slug, without a leading `#`.
pub fn slugify(label: &str) -> String {
    let mut slug = String::with_capacity(label.len());
    for c in label.trim().chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            slug.extend(c.to_lowercase());
        } else if c == ' ' {
            slug.push('-');
        }
    }
    slug
}

/// Decodes `%XX` escapes, as used for spaces and punctuation in link targets.
///
/// Args:
///     raw: A link target or fragment as written in the markdown.
///
/// Returns:
///     The decoded text; malformed escapes are kept literally.
pub fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn list_indent(stack: &[Option<u64>]) -> f32 {
    stack.len() as f32 * LIST_INDENT
}

fn close_style(text: &mut Text, starts: &mut Vec<(Style, u32)>, want: Style) {
    if let Some(pos) = starts.iter().rposition(|(s, _)| *s == want) {
        let (style, start) = starts.remove(pos);
        text.style(start, style);
    }
}

// ---------------------------------------------------------------------------
// Display-list addressing, selection, and link classification
// ---------------------------------------------------------------------------

/// Where a text layout's characters came from, so a hit on the layout can be
/// mapped back to the block text and its links.
#[derive(Clone, Copy)]
pub enum Source {
    Block(usize),
    Cell { block: usize, row: usize, cell: usize },
}

/// A caret position: a text item in the display list and a UTF-16 offset
/// into its text. Ordering follows document order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Caret {
    pub item: usize,
    pub pos: u32,
}

/// A text selection between two carets. `anchor` is where the drag began and
/// `focus` follows the cursor, so either may come first in the document.
#[derive(Clone, Copy)]
pub struct Selection {
    pub anchor: Caret,
    pub focus: Caret,
}

impl Selection {
    /// The selection's endpoints in document order.
    pub fn ordered(&self) -> (Caret, Caret) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }

    /// True when nothing is selected (a click without a drag).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

/// Parts of the search bar a click can land on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BarHit {
    Previous,
    Next,
    Inside,
}

/// What clicking a link does.
pub enum LinkAction {
    /// Scroll to the heading with this anchor.
    Anchor(String),
    /// Hand an http(s) URL to the default browser.
    Browser(String),
    /// Open another markdown file in a new QuickGlass window.
    Document(PathBuf),
    /// Anything else (mailto:, missing files, other schemes) is ignored.
    Ignore,
}

/// Decides what a link destination should do when clicked.
///
/// Only http and https are passed to the shell, so a document cannot launch
/// arbitrary protocol handlers. Relative paths resolve against the open
/// document's folder.
///
/// Args:
///     dest: The link target as written in the markdown.
///     base_dir: Folder of the open document, if it came from a file.
///
/// Returns:
///     The action to take.
pub fn classify_link(dest: &str, base_dir: Option<&Path>) -> LinkAction {
    let dest = dest.trim();
    if let Some(fragment) = dest.strip_prefix('#') {
        return LinkAction::Anchor(fragment.to_owned());
    }
    let lower = dest.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return LinkAction::Browser(dest.to_owned());
    }
    // Any other scheme (mailto:, file:, javascript:, ...) is ignored. A single
    // letter before the colon is a drive letter, not a scheme.
    if let Some((scheme, _)) = dest.split_once(':') {
        if scheme.len() > 1 {
            return LinkAction::Ignore;
        }
    }

    let path_part = dest.split('#').next().unwrap_or_default();
    let path = PathBuf::from(percent_decode(path_part));
    let is_markdown = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"));
    if !is_markdown {
        return LinkAction::Ignore;
    }
    let resolved = match base_dir {
        Some(dir) if path.is_relative() => dir.join(&path),
        _ => path,
    };
    if resolved.is_file() { LinkAction::Document(resolved) } else { LinkAction::Ignore }
}

/// Lowercases one UTF-16 unit when the result is still a single unit.
///
/// Keeping folding 1:1 means match offsets in the folded text are valid
/// offsets in the original, which the highlight hit-test needs.
///
/// Args:
///     unit: A UTF-16 code unit.
///
/// Returns:
///     The folded unit, or `unit` unchanged.
pub fn fold(unit: u16) -> u16 {
    let Some(c) = char::from_u32(u32::from(unit)) else {
        return unit; // surrogate half
    };
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) if u32::from(l) <= 0xFFFF => l as u16,
        _ => unit,
    }
}
