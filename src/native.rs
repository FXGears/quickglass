//! Native markdown renderer built on Direct2D and DirectWrite.
//!
//! Selected with the `--beta_render` flag. This path draws the document
//! directly instead of handing HTML to WebView2, which removes the browser
//! process, the on-disk WebView2 profile, and the HTML/CSS pipeline from
//! startup.
//!
//! Styling is hardcoded to match the CSS used by the WebView2 path, so there
//! is no cascade, selector matching, or general box model here — just a fixed
//! vertical stack of blocks in a single centred column.
//!
//! Known gaps versus the WebView2 renderer, all deliberate for this beta:
//! text selection and clipboard, clickable links, images (alt text is shown
//! instead), auto-sized table columns, and syntax highlighting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pulldown_cmark::{Event as MdEvent, HeadingLevel, Options, Parser, Tag, TagEnd};
use tao::event::{ElementState, Event, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::keyboard::{KeyCode, ModifiersState};
use tao::platform::windows::{IconExtWindows, WindowExtWindows};
use tao::window::{CursorIcon, Icon, WindowBuilder};
use windows::Win32::Foundation::{E_OUTOFMEMORY, GlobalFree, HANDLE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, D2D1_ANTIALIAS_MODE_ALIASED, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_FEATURE_LEVEL_DEFAULT, D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_HIT_TEST_METRICS, DWRITE_LINE_SPACING_METHOD_UNIFORM, DWRITE_TEXT_METRICS,
    DWRITE_TEXT_RANGE, DWRITE_WORD_WRAPPING_NO_WRAP, DWRITE_WORD_WRAPPING_WRAP,
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
    IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{BOOL, HSTRING, PCWSTR, w};
use windows_numerics::Vector2;

// ---------------------------------------------------------------------------
// Theme — mirrors the CSS constants in the WebView2 path.
// ---------------------------------------------------------------------------

const BG: u32 = 0x0d1117;
const FG: u32 = 0xe6edf3;
const FG_STRONG: u32 = 0xf0f6fc;
const FG_MUTED: u32 = 0x8b949e;
const SURFACE: u32 = 0x161b22;
const BORDER: u32 = 0x30363d;
const BORDER_SOFT: u32 = 0x21262d;
const LINK: u32 = 0x58a6ff;

const MAX_COLUMN: f32 = 860.0;
const PAD_X: f32 = 24.0;
const PAD_Y: f32 = 32.0;

const BODY_SIZE: f32 = 16.0;
const BODY_LINE: f32 = 25.6;
const ITEM_LINE: f32 = 27.2;
const CODE_SIZE: f32 = 13.6;
const CODE_LINE: f32 = 19.7;

const BLOCK_GAP: f32 = 16.0;
const HEADING_GAP_ABOVE: f32 = 24.0;
const RULE_GAP: f32 = 24.0;
const CODE_PAD: f32 = 16.0;
const QUOTE_BAR: f32 = 4.0;
const QUOTE_PAD: f32 = 16.0;
const LIST_INDENT: f32 = 32.0;
const ITEM_GAP: f32 = 8.0;
const CELL_PAD_X: f32 = 13.0;
const CELL_PAD_Y: f32 = 6.0;

const THUMB: u32 = 0x30363d;
const THUMB_HOVER: u32 = 0x484f58;
const SCROLLBAR_W: f32 = 10.0;
const THUMB_MIN: f32 = 28.0;

/// Search highlights, drawn translucent under the text (GitHub's find colours).
const MATCH: u32 = 0xbb8009;
const MATCH_ALPHA: f32 = 0.4;
const MATCH_CURRENT_ALPHA: f32 = 0.85;

/// Text selection, translucent under the text (GitHub's selection colour).
const SELECTION: u32 = 0x388bfd;
const SELECTION_ALPHA: f32 = 0.4;

/// Distance, in DIPs, a press must move before it becomes a drag-select
/// rather than a click (which may open a link).
const DRAG_THRESHOLD: f32 = 4.0;

/// Ctrl+F bar, in viewport DIPs, pinned top-right beside the scrollbar.
const BAR_W: f32 = 300.0;
const BAR_H: f32 = 30.0;
const BAR_MARGIN: f32 = 8.0;
const BAR_PAD: f32 = 10.0;
const BAR_ARROW_W: f32 = 22.0;

/// Resource ID of the application icon. `build.rs` embeds `resources/icon.ico`
/// via `winresource::set_icon`, which always uses ID 1.
const ICON_RESOURCE: u16 = 1;

/// Indices into the renderer's brush table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ink {
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
struct Span {
    start: u32,
    len: u32,
    style: Style,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Strong,
    Emphasis,
    Code,
    Link,
}

/// A link destination covering a UTF-16 range of a block's text.
#[derive(Clone)]
struct LinkSpan {
    start: u32,
    len: u32,
    dest: String,
}

/// A run of text with its inline styles, stored as UTF-16 for DirectWrite.
#[derive(Default, Clone)]
struct Text {
    utf16: Vec<u16>,
    spans: Vec<Span>,
    links: Vec<LinkSpan>,
}

impl Text {
    fn push(&mut self, s: &str) {
        self.utf16.extend(s.encode_utf16());
    }

    fn cursor(&self) -> u32 {
        self.utf16.len() as u32
    }

    fn style(&mut self, start: u32, style: Style) {
        let len = self.cursor().saturating_sub(start);
        if len > 0 {
            self.spans.push(Span { start, len, style });
        }
    }

    fn is_blank(&self) -> bool {
        self.utf16.iter().all(|c| *c == 0x20 || *c == 0x0a || *c == 0x09)
    }
}

/// Which text style a block is rendered with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Heading(u8),
    Body,
    Code,
    Item,
}

#[derive(Clone)]
struct TableRow {
    cells: Vec<Text>,
    header: bool,
}

#[derive(Clone)]
enum Block {
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
fn parse(source: &str) -> (Vec<Block>, Vec<(String, usize)>) {
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

            // Images are not decoded in this beta; show the alt text instead.
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
fn slugify(label: &str) -> String {
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
fn percent_decode(raw: &str) -> String {
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

fn list_indent(stack: &[Option<u64>]) -> f32 {
    stack.len() as f32 * LIST_INDENT
}

fn close_style(text: &mut Text, starts: &mut Vec<(Style, u32)>, want: Style) {
    if let Some(pos) = starts.iter().rposition(|(s, _)| *s == want) {
        let (style, start) = starts.remove(pos);
        text.style(start, style);
    }
}

// ---------------------------------------------------------------------------
// Display list
// ---------------------------------------------------------------------------

/// Where a text layout's characters came from, so a hit on the layout can be
/// mapped back to the block text and its links.
#[derive(Clone, Copy)]
enum Source {
    Block(usize),
    Cell { block: usize, row: usize, cell: usize },
}

enum Draw {
    Text { layout: IDWriteTextLayout, x: f32, y: f32, ink: Ink, source: Source },
    /// `radius` of 0 draws square corners.
    Rect { rect: D2D_RECT_F, ink: Ink, radius: f32 },
    Line { x0: f32, y0: f32, x1: f32, y1: f32, ink: Ink, width: f32 },
}

/// A primitive plus the vertical band it occupies, so painting can cull.
struct Item {
    draw: Draw,
    top: f32,
    bottom: f32,
}

/// One occurrence of the search query.
struct Match {
    /// Index of the text item in the display list.
    item: usize,
    /// Highlight rectangles in document coordinates; more than one when the
    /// match wraps across lines.
    rects: Vec<D2D_RECT_F>,
}

/// Ctrl+F state: the query being typed and where it occurs.
struct Search {
    query: String,
    /// Matches in document order.
    matches: Vec<Match>,
    /// Index into `matches` of the highlighted occurrence.
    current: usize,
}

/// A caret position: a text item in the display list and a UTF-16 offset
/// into its text. Ordering follows document order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Caret {
    item: usize,
    pos: u32,
}

/// A text selection between two carets. `anchor` is where the drag began and
/// `focus` follows the cursor, so either may come first in the document.
#[derive(Clone, Copy)]
struct Selection {
    anchor: Caret,
    focus: Caret,
}

impl Selection {
    /// The selection's endpoints in document order.
    fn ordered(&self) -> (Caret, Caret) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }

    /// True when nothing is selected (a click without a drag).
    fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

/// Places text on the clipboard as Unicode text.
///
/// Newlines are converted to CRLF, which is what Windows applications expect
/// when pasting.
///
/// Args:
///     hwnd: Window that owns the clipboard while it is open.
///     text: Text to copy.
///
/// Raises:
///     Returns the Win32 error if the clipboard cannot be opened or written.
fn copy_to_clipboard(hwnd: HWND, text: &str) -> windows::core::Result<()> {
    let mut wide: Vec<u16> =
        text.replace("\r\n", "\n").replace('\n', "\r\n").encode_utf16().collect();
    wide.push(0);
    unsafe {
        OpenClipboard(Some(hwnd))?;
        let result = (|| {
            EmptyClipboard()?;
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * std::mem::size_of::<u16>())?;
            let dest = GlobalLock(memory) as *mut u16;
            if dest.is_null() {
                let _ = GlobalFree(Some(memory));
                return Err(E_OUTOFMEMORY.into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dest, wide.len());
            // Reports an "error" with code 0 once the lock count reaches zero,
            // which is the expected outcome here.
            let _ = GlobalUnlock(memory);
            // On success the clipboard owns the memory; on failure it is ours to free.
            if let Err(error) =
                SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(memory.0)))
            {
                let _ = GlobalFree(Some(memory));
                return Err(error);
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Parts of the search bar a click can land on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BarHit {
    Previous,
    Next,
    Inside,
}

/// What clicking a link does.
enum LinkAction {
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
fn classify_link(dest: &str, base_dir: Option<&Path>) -> LinkAction {
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

/// Opens an http(s) URL in the user's default browser.
///
/// Args:
///     url: An absolute http or https URL; see `classify_link`.
fn open_in_browser(url: &str) {
    let url = HSTRING::from(url);
    unsafe {
        ShellExecuteW(None, w!("open"), &url, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Opens a markdown file in a new QuickGlass process.
///
/// Launches this same executable through `ShellExecuteW`, which is already
/// linked for browser links, rather than `std::process::Command`, whose
/// environment and stdio plumbing would add ~30 KB we never use. The binary
/// is a GUI-subsystem app, so no console window appears.
///
/// Args:
///     path: The file to open.
fn open_document(path: &Path) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // Quoted so paths with spaces arrive as one argument; `"` cannot occur in
    // a Windows file name, so the quoting cannot be broken by the path itself.
    let mut params = std::ffi::OsString::from("\"");
    params.push(path.as_os_str());
    params.push("\"");
    let exe = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(params.as_os_str());
    unsafe {
        ShellExecuteW(None, w!("open"), &exe, &params, PCWSTR::null(), SW_SHOWNORMAL);
    }
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
fn fold(unit: u16) -> u16 {
    let Some(c) = char::from_u32(u32::from(unit)) else {
        return unit; // surrogate half
    };
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) if u32::from(l) <= 0xFFFF => l as u16,
        _ => unit,
    }
}

/// Rectangles covering a text range, offset to document coordinates.
///
/// Args:
///     layout: The text layout containing the range.
///     start: First UTF-16 position of the range.
///     len: Length of the range in UTF-16 units.
///     origin: Document position of the layout's top-left corner.
///
/// Returns:
///     One rectangle per line fragment; empty if DirectWrite fails.
fn range_rects(layout: &IDWriteTextLayout, start: u32, len: u32, origin: (f32, f32)) -> Vec<D2D_RECT_F> {
    let to_rect = |m: &DWRITE_HIT_TEST_METRICS| D2D_RECT_F {
        left: m.left,
        top: m.top,
        right: m.left + m.width,
        bottom: m.top + m.height,
    };
    let mut buf = [DWRITE_HIT_TEST_METRICS::default(); 8];
    let mut count = 0u32;
    unsafe {
        let (ox, oy) = origin;
        if layout.HitTestTextRange(start, len, ox, oy, Some(&mut buf), &mut count).is_ok() {
            return buf.iter().take(count as usize).map(to_rect).collect();
        }
        // More line fragments than the stack buffer holds; `count` is the size needed.
        let mut big = vec![DWRITE_HIT_TEST_METRICS::default(); count as usize];
        if layout.HitTestTextRange(start, len, ox, oy, Some(&mut big), &mut count).is_ok() {
            return big.iter().take(count as usize).map(to_rect).collect();
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

struct Renderer {
    dwrite: IDWriteFactory,
    target: ID2D1HwndRenderTarget,
    brushes: Vec<ID2D1SolidColorBrush>,
    body: IDWriteTextFormat,
    item: IDWriteTextFormat,
    code: IDWriteTextFormat,
    headings: Vec<IDWriteTextFormat>,
    blocks: Vec<Block>,
    items: Vec<Item>,
    /// Heading anchor slugs, each with the index of its block.
    anchors: Vec<(String, usize)>,
    /// Document y of each block's top edge, filled by `relayout`.
    block_tops: Vec<f32>,
    /// Active Ctrl+F search, if the bar is open.
    search: Option<Search>,
    /// Current text selection, if any.
    selection: Option<Selection>,
    /// True while the left button is held and moves extend the selection.
    selecting: bool,
    content_height: f32,
    view: (f32, f32),
    scroll: f32,
    /// Cursor position in DIPs, tracked for scrollbar hover and drag.
    cursor: (f32, f32),
    /// Distance from the top of the thumb to the grab point, while dragging.
    drag_grab: Option<f32>,
    hover_thumb: bool,
}

/// Vertical placement of the scrollbar thumb, in DIPs.
struct Thumb {
    top: f32,
    height: f32,
}

/// Distance from the anchor, in DIPs, within which autoscroll does not move.
const AUTOSCROLL_DEAD_ZONE: f32 = 12.0;
/// Scroll speed in DIPs per second, per DIP of cursor distance past the dead zone.
const AUTOSCROLL_GAIN: f32 = 8.0;
/// Interval between autoscroll steps.
const AUTOSCROLL_FRAME: Duration = Duration::from_millis(16);
/// A middle press held at least this long is treated as press-drag-release
/// rather than click-to-toggle, so releasing the button ends it.
const AUTOSCROLL_HOLD: Duration = Duration::from_millis(300);

/// Middle-click autoscroll in progress, following the Windows convention.
///
/// A middle click anchors at the cursor; moving away from the anchor scrolls at
/// a speed proportional to the distance. Any button, key, or wheel input ends
/// it, as does releasing the middle button after holding it.
struct Autoscroll {
    /// Cursor y, in DIPs, where the middle button was pressed.
    anchor_y: f32,
    /// When the middle button went down, to tell a click from a hold.
    pressed_at: Instant,
    /// Time of the last scroll step, used to scale movement by elapsed time.
    last_step: Instant,
}

impl Autoscroll {
    /// Starts autoscroll anchored at `anchor_y`.
    ///
    /// Args:
    ///     anchor_y: Cursor y in DIPs at the moment of the middle press.
    fn new(anchor_y: f32) -> Self {
        let now = Instant::now();
        Self { anchor_y, pressed_at: now, last_step: now }
    }

    /// Scroll velocity, in DIPs per second, for a cursor at `cursor_y`.
    ///
    /// Args:
    ///     cursor_y: Current cursor y in DIPs.
    ///
    /// Returns:
    ///     Signed velocity; positive scrolls down, zero inside the dead zone.
    fn velocity(&self, cursor_y: f32) -> f32 {
        let offset = cursor_y - self.anchor_y;
        let past = (offset.abs() - AUTOSCROLL_DEAD_ZONE).max(0.0);
        offset.signum() * past * AUTOSCROLL_GAIN
    }

    /// Cursor shape showing the current scroll direction.
    ///
    /// Args:
    ///     cursor_y: Current cursor y in DIPs.
    fn cursor_icon(&self, cursor_y: f32) -> CursorIcon {
        let velocity = self.velocity(cursor_y);
        if velocity > 0.0 {
            CursorIcon::SResize
        } else if velocity < 0.0 {
            CursorIcon::NResize
        } else {
            CursorIcon::NsResize
        }
    }
}

impl Renderer {
    /// Creates the Direct2D target and DirectWrite formats for a window.
    ///
    /// Args:
    ///     hwnd: Target window handle.
    ///     pixels: Window client size in physical pixels.
    ///
    /// Returns:
    ///     A renderer with no document loaded yet.
    fn new(hwnd: isize, pixels: (u32, u32)) -> windows::core::Result<Self> {
        unsafe {
            let factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;

            let target_props = D2D1_RENDER_TARGET_PROPERTIES {
                // Software, not hardware: creating a hardware target spins up a
                // Direct3D device and the GPU driver, measured at ~180 ms of the
                // ~230 ms startup. Software creation is ~22 ms, and first paint
                // is no slower for a page of static text.
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                // Zero means "use the system DPI", which makes every coordinate
                // below a DIP and gives correct HiDPI scaling for free.
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd: HWND(hwnd as *mut core::ffi::c_void),
                pixelSize: D2D_SIZE_U { width: pixels.0.max(1), height: pixels.1.max(1) },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let target = factory.CreateHwndRenderTarget(&target_props, &hwnd_props)?;

            let mut brushes = Vec::new();
            for rgb in [
                BG,
                FG,
                FG_STRONG,
                FG_MUTED,
                SURFACE,
                BORDER,
                BORDER_SOFT,
                LINK,
                THUMB,
                THUMB_HOVER,
            ] {
                brushes.push(target.CreateSolidColorBrush(&color(rgb), None)?);
            }
            for alpha in [MATCH_ALPHA, MATCH_CURRENT_ALPHA] {
                let mut tint = color(MATCH);
                tint.a = alpha;
                brushes.push(target.CreateSolidColorBrush(&tint, None)?);
            }
            let mut selection = color(SELECTION);
            selection.a = SELECTION_ALPHA;
            brushes.push(target.CreateSolidColorBrush(&selection, None)?);

            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let body = make_format(&dwrite, w!("Segoe UI"), BODY_SIZE, BODY_LINE, false)?;
            let item = make_format(&dwrite, w!("Segoe UI"), BODY_SIZE, ITEM_LINE, false)?;
            let code = make_format(&dwrite, w!("Consolas"), CODE_SIZE, CODE_LINE, false)?;

            let mut headings = Vec::new();
            for size in [32.0_f32, 24.0, 20.0, 16.0, 16.0, 16.0] {
                headings.push(make_format(
                    &dwrite,
                    w!("Segoe UI"),
                    size,
                    size * 1.25,
                    true,
                )?);
            }

            Ok(Self {
                dwrite,
                target,
                brushes,
                body,
                item,
                code,
                headings,
                blocks: Vec::new(),
                items: Vec::new(),
                anchors: Vec::new(),
                block_tops: Vec::new(),
                search: None,
                selection: None,
                selecting: false,
                content_height: 0.0,
                view: (0.0, 0.0),
                scroll: 0.0,
                cursor: (-1.0, -1.0),
                drag_grab: None,
                hover_thumb: false,
            })
        }
    }

    fn brush(&self, ink: Ink) -> &ID2D1SolidColorBrush {
        let index = match ink {
            Ink::Background => 0,
            Ink::Text => 1,
            Ink::Strong => 2,
            Ink::Muted => 3,
            Ink::Surface => 4,
            Ink::Border => 5,
            Ink::BorderSoft => 6,
            Ink::Link => 7,
            Ink::Thumb => 8,
            Ink::ThumbHover => 9,
            Ink::Match => 10,
            Ink::MatchCurrent => 11,
            Ink::Selection => 12,
        };
        &self.brushes[index]
    }

    /// Builds a styled text layout for one run of text.
    fn build_layout(
        &self,
        text: &Text,
        format: &IDWriteTextFormat,
        width: f32,
    ) -> windows::core::Result<(IDWriteTextLayout, f32)> {
        unsafe {
            let layout = self.dwrite.CreateTextLayout(&text.utf16, format, width, f32::MAX)?;
            layout.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;

            for span in &text.spans {
                let range = DWRITE_TEXT_RANGE { startPosition: span.start, length: span.len };
                match span.style {
                    Style::Strong => {
                        layout.SetFontWeight(DWRITE_FONT_WEIGHT_SEMI_BOLD, range)?;
                        layout.SetDrawingEffect(self.brush(Ink::Strong), range)?;
                    }
                    Style::Emphasis => layout.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, range)?,
                    Style::Code => {
                        layout.SetFontFamilyName(w!("Consolas"), range)?;
                        layout.SetFontSize(CODE_SIZE, range)?;
                    }
                    Style::Link => {
                        layout.SetDrawingEffect(self.brush(Ink::Link), range)?;
                    }
                }
            }

            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics)?;
            Ok((layout, metrics.height))
        }
    }

    /// Rebuilds the display list for the current viewport width.
    ///
    /// Args:
    ///     view: Client size in DIPs.
    fn relayout(&mut self, view: (f32, f32)) -> windows::core::Result<()> {
        self.view = view;
        self.items.clear();

        // The scrollbar always reserves its width so text never sits underneath
        // it, matching how a browser gutter behaves.
        let usable = (view.0 - SCROLLBAR_W).max(120.0);
        let column = usable.min(MAX_COLUMN);
        let text_width = (column - PAD_X * 2.0).max(80.0);
        let origin_x = ((usable - column) / 2.0 + PAD_X).max(0.0);
        let mut y = PAD_Y;
        let blocks = std::mem::take(&mut self.blocks);
        self.block_tops = vec![0.0; blocks.len()];

        for (index, block) in blocks.iter().enumerate() {
            match block {
                Block::Rule => {
                    y += RULE_GAP;
                    self.items.push(Item {
                        draw: Draw::Line {
                            x0: origin_x,
                            y0: y,
                            x1: origin_x + text_width,
                            y1: y,
                            ink: Ink::BorderSoft,
                            width: 1.0,
                        },
                        top: y - 1.0,
                        bottom: y + 1.0,
                    });
                    y += RULE_GAP;
                }

                Block::Text { flavor, text, indent, quote_depth, tight } => {
                    let quote_inset = *quote_depth as f32 * (QUOTE_BAR + QUOTE_PAD);
                    let x = origin_x + indent + quote_inset;
                    let avail = (text_width - indent - quote_inset).max(60.0);

                    let gap = match flavor {
                        Flavor::Heading(_) if index > 0 => HEADING_GAP_ABOVE,
                        Flavor::Item if *tight => ITEM_GAP,
                        _ if index > 0 => BLOCK_GAP,
                        _ => 0.0,
                    };
                    y += gap;
                    self.block_tops[index] = y;

                    let (format, ink) = match flavor {
                        Flavor::Heading(n) => {
                            (self.headings[(*n as usize - 1).min(5)].clone(), Ink::Strong)
                        }
                        Flavor::Code => (self.code.clone(), Ink::Text),
                        Flavor::Item => (self.item.clone(), Ink::Text),
                        Flavor::Body => (self.body.clone(), Ink::Text),
                    };
                    let ink = if *quote_depth > 0 { Ink::Muted } else { ink };

                    let inner = if *flavor == Flavor::Code { avail - CODE_PAD * 2.0 } else { avail };
                    let (layout, height) = self.build_layout(text, &format, inner.max(40.0))?;

                    if *flavor == Flavor::Code {
                        let rect = D2D_RECT_F {
                            left: x,
                            top: y,
                            right: x + avail,
                            bottom: y + height + CODE_PAD * 2.0,
                        };
                        self.items.push(Item {
                            draw: Draw::Rect { rect, ink: Ink::Surface, radius: 6.0 },
                            top: rect.top,
                            bottom: rect.bottom,
                        });
                        self.items.push(Item {
                            draw: Draw::Text {
                                layout,
                                x: x + CODE_PAD,
                                y: y + CODE_PAD,
                                ink,
                                source: Source::Block(index),
                            },
                            top: y,
                            bottom: y + height + CODE_PAD * 2.0,
                        });
                        y += height + CODE_PAD * 2.0;
                    } else {
                        for depth in 0..*quote_depth {
                            let bar_x = origin_x + indent + depth as f32 * (QUOTE_BAR + QUOTE_PAD);
                            let rect = D2D_RECT_F {
                                left: bar_x,
                                top: y,
                                right: bar_x + QUOTE_BAR,
                                bottom: y + height,
                            };
                            self.items.push(Item {
                                draw: Draw::Rect { rect, ink: Ink::Border, radius: 0.0 },
                                top: rect.top,
                                bottom: rect.bottom,
                            });
                        }
                        self.items.push(Item {
                            draw: Draw::Text { layout, x, y, ink, source: Source::Block(index) },
                            top: y,
                            bottom: y + height,
                        });
                        y += height;

                        // h1 and h2 carry a bottom rule in the CSS.
                        if let Flavor::Heading(n) = flavor {
                            if *n <= 2 {
                                y += 0.3 * if *n == 1 { 32.0 } else { 24.0 };
                                self.items.push(Item {
                                    draw: Draw::Line {
                                        x0: x,
                                        y0: y,
                                        x1: origin_x + text_width,
                                        y1: y,
                                        ink: Ink::BorderSoft,
                                        width: 1.0,
                                    },
                                    top: y - 1.0,
                                    bottom: y + 1.0,
                                });
                            }
                        }
                    }
                }

                Block::Table(rows) => {
                    y += BLOCK_GAP;
                    let columns = rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
                    if columns == 0 {
                        continue;
                    }
                    // Equal column widths. Real auto-width table layout is not
                    // implemented in this beta.
                    let col_width = text_width / columns as f32;
                    let cell_width = (col_width - CELL_PAD_X * 2.0).max(30.0);
                    let table_top = y;

                    for (row_index, row) in rows.iter().enumerate() {
                        let mut laid = Vec::new();
                        let mut row_height: f32 = 0.0;
                        for cell in &row.cells {
                            let format =
                                if row.header { self.headings[5].clone() } else { self.body.clone() };
                            let (layout, height) = self.build_layout(cell, &format, cell_width)?;
                            row_height = row_height.max(height);
                            laid.push(layout);
                        }
                        let row_bottom = y + row_height + CELL_PAD_Y * 2.0;

                        if row.header {
                            self.items.push(Item {
                                draw: Draw::Rect {
                                    rect: D2D_RECT_F {
                                        left: origin_x,
                                        top: y,
                                        right: origin_x + text_width,
                                        bottom: row_bottom,
                                    },
                                    ink: Ink::Surface,
                                    radius: 0.0,
                                },
                                top: y,
                                bottom: row_bottom,
                            });
                        }

                        for (column, layout) in laid.into_iter().enumerate() {
                            self.items.push(Item {
                                draw: Draw::Text {
                                    layout,
                                    x: origin_x + column as f32 * col_width + CELL_PAD_X,
                                    y: y + CELL_PAD_Y,
                                    ink: if row.header { Ink::Strong } else { Ink::Text },
                                    source: Source::Cell { block: index, row: row_index, cell: column },
                                },
                                top: y,
                                bottom: row_bottom,
                            });
                        }

                        // Horizontal rule under the row.
                        self.items.push(Item {
                            draw: Draw::Line {
                                x0: origin_x,
                                y0: row_bottom,
                                x1: origin_x + text_width,
                                y1: row_bottom,
                                ink: Ink::Border,
                                width: 1.0,
                            },
                            top: row_bottom - 1.0,
                            bottom: row_bottom + 1.0,
                        });
                        y = row_bottom;
                    }

                    // Column separators and outer edges.
                    for column in 0..=columns {
                        let x = origin_x + column as f32 * col_width;
                        self.items.push(Item {
                            draw: Draw::Line {
                                x0: x,
                                y0: table_top,
                                x1: x,
                                y1: y,
                                ink: Ink::Border,
                                width: 1.0,
                            },
                            top: table_top,
                            bottom: y,
                        });
                    }
                    self.items.push(Item {
                        draw: Draw::Line {
                            x0: origin_x,
                            y0: table_top,
                            x1: origin_x + text_width,
                            y1: table_top,
                            ink: Ink::Border,
                            width: 1.0,
                        },
                        top: table_top - 1.0,
                        bottom: table_top + 1.0,
                    });
                }
            }
        }

        self.blocks = blocks;
        self.content_height = y + PAD_Y;
        self.clamp_scroll();
        // Layouts were rebuilt, so highlight rectangles must be recomputed.
        self.rematch(false);
        Ok(())
    }

    fn max_scroll(&self) -> f32 {
        // Callers bound scroll with `.max(0.0).min(self.max_scroll())` rather than
        // `f32::clamp`: clamp's `min <= max` assertion formats floats in its panic
        // message, which links float formatting into the binary. The lower bound
        // is 0 and this is never negative, so the results are identical.
        (self.content_height - self.view.1).max(0.0)
    }

    /// Thumb geometry, or `None` when the document fits and no bar is needed.
    fn thumb(&self) -> Option<Thumb> {
        let max = self.max_scroll();
        if max <= 0.0 || self.view.1 <= 0.0 {
            return None;
        }
        let track = self.view.1;
        let visible = (track / self.content_height).max(0.0).min(1.0);
        let height = (track * visible).max(THUMB_MIN).min(track);
        let top = (self.scroll / max) * (track - height);
        Some(Thumb { top, height })
    }

    /// True when `x` falls inside the scrollbar gutter.
    fn in_gutter(&self, x: f32) -> bool {
        x >= self.view.0 - SCROLLBAR_W
    }

    /// Recomputes hover state, reporting whether it changed.
    fn update_hover(&mut self) -> bool {
        let over = match self.thumb() {
            Some(t) => {
                self.in_gutter(self.cursor.0)
                    && self.cursor.1 >= t.top
                    && self.cursor.1 <= t.top + t.height
            }
            None => false,
        };
        let changed = over != self.hover_thumb;
        self.hover_thumb = over;
        changed
    }

    /// Begins a drag, or jumps the thumb if the click landed on bare track.
    ///
    /// Args:
    ///     y: Click position in DIPs.
    ///
    /// Returns:
    ///     True when the view needs repainting.
    fn press_gutter(&mut self, y: f32) -> bool {
        let Some(t) = self.thumb() else {
            return false;
        };
        if y >= t.top && y <= t.top + t.height {
            self.drag_grab = Some(y - t.top);
            self.hover_thumb = true;
            true
        } else {
            // Centre the thumb on the click, then continue as a drag so the
            // user can keep adjusting without releasing.
            self.drag_grab = Some(t.height / 2.0);
            self.drag_to(y);
            self.hover_thumb = true;
            true
        }
    }

    /// Maps a cursor position to a scroll offset while dragging.
    fn drag_to(&mut self, y: f32) -> bool {
        let Some(grab) = self.drag_grab else {
            return false;
        };
        let Some(t) = self.thumb() else {
            return false;
        };
        let span = self.view.1 - t.height;
        if span <= 0.0 {
            return false;
        }
        let target = ((y - grab) / span) * self.max_scroll();
        let before = self.scroll;
        self.scroll = target.max(0.0).min(self.max_scroll());
        self.scroll != before
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.max(0.0).min(self.max_scroll());
    }

    /// Scrolls by `delta` DIPs and reports whether the offset actually moved.
    fn scroll_by(&mut self, delta: f32) -> bool {
        let before = self.scroll;
        self.scroll = (self.scroll + delta).max(0.0).min(self.max_scroll());
        self.scroll != before
    }

    /// Scrolls to an absolute document offset, reporting whether it moved.
    fn scroll_to(&mut self, offset: f32) -> bool {
        let before = self.scroll;
        self.scroll = offset.max(0.0).min(self.max_scroll());
        self.scroll != before
    }

    /// Returns the source text behind a text layout.
    fn text_of(&self, source: Source) -> Option<&Text> {
        match source {
            Source::Block(block) => match self.blocks.get(block)? {
                Block::Text { text, .. } => Some(text),
                _ => None,
            },
            Source::Cell { block, row, cell } => match self.blocks.get(block)? {
                Block::Table(rows) => rows.get(row)?.cells.get(cell),
                _ => None,
            },
        }
    }

    /// Finds the character under a viewport point.
    ///
    /// Args:
    ///     point: Position in viewport DIPs.
    ///
    /// Returns:
    ///     The display-list index of the text item and the UTF-16 position
    ///     of the character, when the point lies on text.
    fn hit_text(&self, point: (f32, f32)) -> Option<(usize, u32)> {
        let doc_y = point.1 + self.scroll;
        for (index, item) in self.items.iter().enumerate() {
            if doc_y < item.top || doc_y > item.bottom {
                continue;
            }
            let Draw::Text { layout, x, y, .. } = &item.draw else {
                continue;
            };
            let mut trailing = BOOL(0);
            let mut inside = BOOL(0);
            let mut metrics = DWRITE_HIT_TEST_METRICS::default();
            unsafe {
                layout
                    .HitTestPoint(point.0 - x, doc_y - y, &mut trailing, &mut inside, &mut metrics)
                    .ok()?;
            }
            if inside.as_bool() {
                return Some((index, metrics.textPosition));
            }
        }
        None
    }

    /// Text length, in UTF-16 units, of the text item at `index`.
    fn item_len(&self, index: usize) -> u32 {
        match self.items.get(index).map(|item| &item.draw) {
            Some(Draw::Text { source, .. }) => {
                self.text_of(*source).map_or(0, |text| text.utf16.len() as u32)
            }
            _ => 0,
        }
    }

    /// Caret position nearest a viewport point, for starting or extending a
    /// selection.
    ///
    /// Unlike `hit_text`, this always resolves to some text when the document
    /// has any: a point between blocks snaps to the nearest one vertically,
    /// and a point beside table cells snaps to the cell at or left of it.
    ///
    /// Args:
    ///     point: Position in viewport DIPs; may lie outside the window.
    fn caret_at(&self, point: (f32, f32)) -> Option<Caret> {
        let doc_y = point.1 + self.scroll;
        // Rank by vertical distance, then distance to the left edge for cells to
        // the right of the point, then prefer the rightmost cell at or left of it.
        let mut best: Option<(usize, f32, f32, f32)> = None;
        for (index, item) in self.items.iter().enumerate() {
            let Draw::Text { x, .. } = &item.draw else {
                continue;
            };
            let dy = (item.top - doc_y).max(doc_y - item.bottom).max(0.0);
            let dx = (x - point.0).max(0.0);
            let better = match best {
                None => true,
                Some((_, bdy, bdx, bx)) => (dy, dx, -x) < (bdy, bdx, -bx),
            };
            if better {
                best = Some((index, dy, dx, *x));
            }
        }
        let (index, ..) = best?;
        let Draw::Text { layout, x, y, .. } = &self.items[index].draw else {
            return None;
        };
        let mut trailing = BOOL(0);
        let mut inside = BOOL(0);
        let mut metrics = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            layout
                .HitTestPoint(point.0 - x, doc_y - y, &mut trailing, &mut inside, &mut metrics)
                .ok()?;
        }
        let pos = metrics.textPosition + if trailing.as_bool() { metrics.length } else { 0 };
        Some(Caret { item: index, pos: pos.min(self.item_len(index)) })
    }

    /// Selects every piece of text in the document.
    fn select_all(&mut self) {
        let mut texts = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| matches!(item.draw, Draw::Text { .. }))
            .map(|(index, _)| index);
        let Some(first) = texts.next() else {
            return;
        };
        let last = texts.last().unwrap_or(first);
        self.selection = Some(Selection {
            anchor: Caret { item: first, pos: 0 },
            focus: Caret { item: last, pos: self.item_len(last) },
        });
    }

    /// The selected text, as it should be pasted elsewhere.
    ///
    /// Blocks are separated by newlines; cells in the same table row by tabs,
    /// so a copied table pastes into a spreadsheet as rows and columns.
    fn selected_text(&self) -> String {
        let Some(selection) = self.selection.filter(|s| !s.is_empty()) else {
            return String::new();
        };
        let (start, end) = selection.ordered();
        let mut out: Vec<u16> = Vec::new();
        let mut previous: Option<Source> = None;

        for index in start.item..=end.item {
            let Some(Draw::Text { source, .. }) = self.items.get(index).map(|item| &item.draw) else {
                continue;
            };
            let Some(text) = self.text_of(*source) else {
                continue;
            };
            let len = text.utf16.len() as u32;
            let from = if index == start.item { start.pos.min(len) } else { 0 };
            let to = if index == end.item { end.pos.min(len) } else { len };

            if let Some(previous) = previous {
                let same_row = matches!(
                    (previous, *source),
                    (Source::Cell { block: b1, row: r1, .. }, Source::Cell { block: b2, row: r2, .. })
                        if b1 == b2 && r1 == r2
                );
                out.push(if same_row { u16::from(b'\t') } else { u16::from(b'\n') });
            }
            if from < to {
                out.extend_from_slice(&text.utf16[from as usize..to as usize]);
            }
            previous = Some(*source);
        }
        String::from_utf16_lossy(&out)
    }

    /// Selected range within one text item, if the selection covers it.
    fn selected_range(&self, index: usize) -> Option<(u32, u32)> {
        let selection = self.selection.filter(|s| !s.is_empty())?;
        let (start, end) = selection.ordered();
        if index < start.item || index > end.item {
            return None;
        }
        let from = if index == start.item { start.pos } else { 0 };
        let to = if index == end.item { end.pos } else { self.item_len(index) };
        (from < to).then_some((from, to - from))
    }

    /// Link destination under a viewport point, if any.
    ///
    /// Args:
    ///     point: Position in viewport DIPs.
    fn link_at(&self, point: (f32, f32)) -> Option<&str> {
        if self.in_gutter(point.0) || self.bar_hit(point).is_some() {
            return None;
        }
        let (index, position) = self.hit_text(point)?;
        let Draw::Text { source, .. } = &self.items[index].draw else {
            return None;
        };
        self.text_of(*source)?
            .links
            .iter()
            .find(|link| position >= link.start && position < link.start + link.len)
            .map(|link| link.dest.as_str())
    }

    /// Scrolls so the heading with the given anchor sits at the top.
    ///
    /// Matching ignores case so hand-written tables of contents still work.
    ///
    /// Args:
    ///     fragment: The part of the link after `#`, possibly percent-encoded.
    ///
    /// Returns:
    ///     True when the view needs repainting.
    fn jump_to_anchor(&mut self, fragment: &str) -> bool {
        let wanted = percent_decode(fragment).to_lowercase();
        let Some(&(_, block)) = self.anchors.iter().find(|(slug, _)| slug.to_lowercase() == wanted)
        else {
            return false;
        };
        let top = self.block_tops.get(block).copied().unwrap_or(0.0);
        self.scroll_to(top - BLOCK_GAP)
    }

    /// Opens the search bar, keeping any query already typed.
    fn open_search(&mut self) {
        if self.search.is_none() {
            self.search = Some(Search { query: String::new(), matches: Vec::new(), current: 0 });
        }
    }

    /// Applies an edit to the query and re-runs the search.
    ///
    /// Args:
    ///     edit: Mutates the query string in place.
    fn edit_query(&mut self, edit: impl FnOnce(&mut String)) {
        if let Some(search) = self.search.as_mut() {
            edit(&mut search.query);
            self.rematch(true);
        }
    }

    /// Recomputes matches and their highlight rectangles.
    ///
    /// Args:
    ///     from_view: When true (the query changed), select the first match at
    ///         or below the current scroll position and bring it into view.
    ///         When false (a relayout), keep the current index.
    fn rematch(&mut self, from_view: bool) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        let needle: Vec<u16> = search.query.encode_utf16().map(fold).collect();
        let mut matches = Vec::new();

        if !needle.is_empty() {
            for (index, item) in self.items.iter().enumerate() {
                let Draw::Text { layout, x, y, source, .. } = &item.draw else {
                    continue;
                };
                let Some(text) = self.text_of(*source) else {
                    continue;
                };
                let haystack: Vec<u16> = text.utf16.iter().copied().map(fold).collect();
                let mut at = 0;
                while at + needle.len() <= haystack.len() {
                    if haystack[at..at + needle.len()] == needle[..] {
                        let rects = range_rects(layout, at as u32, needle.len() as u32, (*x, *y));
                        matches.push(Match { item: index, rects });
                        at += needle.len();
                    } else {
                        at += 1;
                    }
                }
            }
        }

        let previous = search.current;
        let current = if from_view {
            let view_top = self.scroll;
            matches
                .iter()
                .position(|m| m.rects.first().is_some_and(|r| r.top >= view_top))
                .unwrap_or(0)
        } else {
            previous.min(matches.len().saturating_sub(1))
        };

        if let Some(search) = self.search.as_mut() {
            search.matches = matches;
            search.current = current;
        }
        if from_view {
            self.reveal_current();
        }
    }

    /// Moves to the next or previous match, wrapping at either end.
    ///
    /// Args:
    ///     forward: True for the next match, false for the previous one.
    fn step_search(&mut self, forward: bool) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let count = search.matches.len();
        if count == 0 {
            return;
        }
        search.current = if forward {
            (search.current + 1) % count
        } else {
            (search.current + count - 1) % count
        };
        self.reveal_current();
    }

    /// Scrolls the current match into view if it is hidden or under the bar.
    fn reveal_current(&mut self) {
        let Some(rect) = self
            .search
            .as_ref()
            .and_then(|s| s.matches.get(s.current))
            .and_then(|m| m.rects.first().copied())
        else {
            return;
        };
        let visible_top = self.scroll + BAR_MARGIN + BAR_H + BAR_MARGIN;
        let visible_bottom = self.scroll + self.view.1 - BAR_MARGIN;
        if rect.top < visible_top || rect.bottom > visible_bottom {
            self.scroll_to(rect.top - self.view.1 / 3.0);
        }
    }

    /// Search bar rectangle in viewport DIPs.
    fn bar_rect(&self) -> D2D_RECT_F {
        let right = self.view.0 - SCROLLBAR_W - BAR_MARGIN;
        D2D_RECT_F {
            left: (right - BAR_W).max(BAR_MARGIN),
            top: BAR_MARGIN,
            right,
            bottom: BAR_MARGIN + BAR_H,
        }
    }

    /// True when the up/down arrows are shown, i.e. there is more than one match.
    fn bar_has_arrows(&self) -> bool {
        self.search.as_ref().is_some_and(|s| s.matches.len() > 1)
    }

    /// Which part of the open search bar a viewport point falls on.
    ///
    /// Args:
    ///     point: Position in viewport DIPs.
    ///
    /// Returns:
    ///     `None` when the bar is closed or the point is outside it.
    fn bar_hit(&self, point: (f32, f32)) -> Option<BarHit> {
        self.search.as_ref()?;
        let bar = self.bar_rect();
        let (x, y) = point;
        if x < bar.left || x > bar.right || y < bar.top || y > bar.bottom {
            return None;
        }
        if self.bar_has_arrows() {
            if x >= bar.right - BAR_ARROW_W {
                return Some(BarHit::Next);
            }
            if x >= bar.right - BAR_ARROW_W * 2.0 {
                return Some(BarHit::Previous);
            }
        }
        Some(BarHit::Inside)
    }

    /// Creates a single-line text layout for the search bar.
    fn bar_layout(&self, text: &str, size: f32) -> windows::core::Result<(IDWriteTextLayout, f32, f32)> {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let layout = self.dwrite.CreateTextLayout(&utf16, &self.body, f32::MAX, BAR_H)?;
            layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let all = DWRITE_TEXT_RANGE { startPosition: 0, length: utf16.len() as u32 };
            layout.SetFontSize(size, all)?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics)?;
            Ok((layout, metrics.widthIncludingTrailingWhitespace, metrics.height))
        }
    }

    /// Draws the search bar: query with caret, match count, and arrows.
    fn paint_bar(&self, search: &Search) -> windows::core::Result<()> {
        let bar = self.bar_rect();
        let mid = (bar.top + bar.bottom) / 2.0;
        unsafe {
            let outline = D2D1_ROUNDED_RECT { rect: bar, radiusX: 6.0, radiusY: 6.0 };
            self.target.FillRoundedRectangle(&outline, self.brush(Ink::Surface));
            self.target.DrawRoundedRectangle(&outline, self.brush(Ink::Border), 1.0, None);

            let mut right = bar.right - BAR_PAD;
            if self.bar_has_arrows() {
                for (glyph, slot) in [("\u{25B2}", 2.0), ("\u{25BC}", 1.0)] {
                    let (layout, width, height) = self.bar_layout(glyph, 10.0)?;
                    let x = bar.right - BAR_ARROW_W * slot + (BAR_ARROW_W - width) / 2.0;
                    self.target.DrawTextLayout(
                        Vector2 { X: x, Y: mid - height / 2.0 },
                        &layout,
                        self.brush(Ink::Muted),
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
                right = bar.right - BAR_ARROW_W * 2.0 - 4.0;
            }

            if !search.query.is_empty() {
                let count = if search.matches.is_empty() {
                    String::from("0/0")
                } else {
                    format!("{}/{}", search.current + 1, search.matches.len())
                };
                let (layout, width, height) = self.bar_layout(&count, 13.0)?;
                right -= width;
                self.target.DrawTextLayout(
                    Vector2 { X: right, Y: mid - height / 2.0 },
                    &layout,
                    self.brush(Ink::Muted),
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
                right -= 8.0;
            }

            // Query, clipped to its field and scrolled so the caret stays visible.
            let field_left = bar.left + BAR_PAD;
            let field_width = (right - field_left).max(10.0);
            let (layout, width, height) = self.bar_layout(&search.query, 14.0)?;
            let shift = (width - field_width + 2.0).max(0.0);
            let clip = D2D_RECT_F { left: field_left, top: bar.top, right: field_left + field_width, bottom: bar.bottom };
            self.target.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
            self.target.DrawTextLayout(
                Vector2 { X: field_left - shift, Y: mid - height / 2.0 },
                &layout,
                self.brush(Ink::Text),
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            let caret_x = field_left - shift + width + 1.0;
            self.target.DrawLine(
                Vector2 { X: caret_x, Y: bar.top + 7.0 },
                Vector2 { X: caret_x, Y: bar.bottom - 7.0 },
                self.brush(Ink::Text),
                1.0,
                None,
            );
            self.target.PopAxisAlignedClip();
        }
        Ok(())
    }

    /// Draws the visible slice of the display list.
    fn paint(&self) -> windows::core::Result<()> {
        unsafe {
            self.target.BeginDraw();
            self.target.Clear(Some(&color(BG)));

            let top = self.scroll;
            let bottom = self.scroll + self.view.1;
            let search = self.search.as_ref();
            // Matches are in display-list order, so one cursor walks them in step
            // with the items.
            let mut next_match = 0usize;

            for (index, item) in self.items.iter().enumerate() {
                if item.bottom < top || item.top > bottom {
                    continue;
                }
                if let (Some(search), Draw::Text { .. }) = (search, &item.draw) {
                    while next_match < search.matches.len() && search.matches[next_match].item < index {
                        next_match += 1;
                    }
                    let mut k = next_match;
                    while k < search.matches.len() && search.matches[k].item == index {
                        let ink = if k == search.current { Ink::MatchCurrent } else { Ink::Match };
                        for rect in &search.matches[k].rects {
                            let shifted = D2D_RECT_F {
                                left: rect.left,
                                top: rect.top - self.scroll,
                                right: rect.right,
                                bottom: rect.bottom - self.scroll,
                            };
                            self.target.FillRectangle(&shifted, self.brush(ink));
                        }
                        k += 1;
                    }
                }
                if let Draw::Text { layout, x, y, .. } = &item.draw {
                    if let Some((start, len)) = self.selected_range(index) {
                        for rect in range_rects(layout, start, len, (*x, *y - self.scroll)) {
                            self.target.FillRectangle(&rect, self.brush(Ink::Selection));
                        }
                    }
                }
                match &item.draw {
                    Draw::Text { layout, x, y, ink, .. } => {
                        self.target.DrawTextLayout(
                            Vector2 { X: *x, Y: *y - self.scroll },
                            layout,
                            self.brush(*ink),
                            D2D1_DRAW_TEXT_OPTIONS_NONE,
                        );
                    }
                    Draw::Rect { rect, ink, radius } => {
                        let shifted = D2D_RECT_F {
                            left: rect.left,
                            top: rect.top - self.scroll,
                            right: rect.right,
                            bottom: rect.bottom - self.scroll,
                        };
                        if *radius > 0.0 {
                            self.target.FillRoundedRectangle(
                                &D2D1_ROUNDED_RECT {
                                    rect: shifted,
                                    radiusX: *radius,
                                    radiusY: *radius,
                                },
                                self.brush(*ink),
                            );
                        } else {
                            self.target.FillRectangle(&shifted, self.brush(*ink));
                        }
                    }
                    Draw::Line { x0, y0, x1, y1, ink, width } => {
                        self.target.DrawLine(
                            Vector2 { X: *x0, Y: *y0 - self.scroll },
                            Vector2 { X: *x1, Y: *y1 - self.scroll },
                            self.brush(*ink),
                            *width,
                            None,
                        );
                    }
                }
            }

            // Scrollbar last so it sits above content, and unculled since it is
            // in viewport space rather than document space.
            if let Some(t) = self.thumb() {
                let left = self.view.0 - SCROLLBAR_W;
                self.target.FillRectangle(
                    &D2D_RECT_F {
                        left,
                        top: 0.0,
                        right: self.view.0,
                        bottom: self.view.1,
                    },
                    self.brush(Ink::Background),
                );

                let inset = 1.0;
                let radius = (SCROLLBAR_W - inset * 2.0) / 2.0;
                let ink = if self.hover_thumb || self.drag_grab.is_some() {
                    Ink::ThumbHover
                } else {
                    Ink::Thumb
                };
                self.target.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: left + inset,
                            top: t.top,
                            right: self.view.0 - inset,
                            bottom: t.top + t.height,
                        },
                        radiusX: radius,
                        radiusY: radius,
                    },
                    self.brush(ink),
                );
            }

            if let Some(search) = search {
                // A failed bar draw must not skip EndDraw below.
                let _ = self.paint_bar(search);
            }

            self.target.EndDraw(None, None)?;
            Ok(())
        }
    }

    fn resize(&mut self, pixels: (u32, u32)) -> windows::core::Result<()> {
        unsafe {
            self.target.Resize(&D2D_SIZE_U {
                width: pixels.0.max(1),
                height: pixels.1.max(1),
            })?;
        }
        Ok(())
    }
}

fn color(rgb: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 0xff) as f32 / 255.0,
        g: ((rgb >> 8) & 0xff) as f32 / 255.0,
        b: (rgb & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

fn make_format(
    dwrite: &IDWriteFactory,
    family: windows::core::PCWSTR,
    size: f32,
    line: f32,
    strong: bool,
) -> windows::core::Result<IDWriteTextFormat> {
    unsafe {
        let weight = if strong { DWRITE_FONT_WEIGHT_SEMI_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL };
        let format = dwrite.CreateTextFormat(
            family,
            None::<&IDWriteFontCollection>,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("en-us"),
        )?;
        // Uniform line spacing so leading matches the CSS line-height rather
        // than whatever the font's own metrics suggest.
        format.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, line, line * 0.8)?;
        Ok(format)
    }
}

/// Ends autoscroll, if active, and restores the normal cursor and event wait.
///
/// Args:
///     autoscroll: Current autoscroll state; cleared on return.
///     window: Window whose cursor is restored.
///     control_flow: Event loop control; set back to plain waiting.
///
/// Returns:
///     `true` if autoscroll was active, so the caller can swallow the input
///     that ended it.
fn stop_autoscroll(
    autoscroll: &mut Option<Autoscroll>,
    window: &tao::window::Window,
    control_flow: &mut ControlFlow,
) -> bool {
    if autoscroll.take().is_none() {
        return false;
    }
    window.set_cursor_icon(CursorIcon::Default);
    *control_flow = ControlFlow::Wait;
    true
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Opens a window and renders `source` with Direct2D until the user closes it.
///
/// Args:
///     source: Markdown text to display.
///     title: Window title.
///     path: File the markdown came from, used to resolve relative links.
///     trace: Startup trace shared with `main`, so both renderers report
///         timings on the same clock.
///
/// Returns:
///     Never; the process exits when the window is closed.
pub fn run(
    source: &str,
    title: &str,
    path: Option<&Path>,
    trace: std::rc::Rc<std::cell::RefCell<crate::StartupTrace>>,
) -> ! {
    let base_dir: Option<PathBuf> = path.and_then(Path::parent).map(Path::to_path_buf);
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(title)
        .with_background_color((13, 17, 23, 255))
        .with_visible(false)
        .with_inner_size(tao::dpi::LogicalSize::new(920.0, 700.0))
        .build(&event_loop)
        .expect("Failed to create window");

    let physical = window.inner_size();
    let scale = window.scale_factor();
    let logical = physical.to_logical::<f32>(scale);

    // Icons come from the `.ico` already embedded in the exe, requested at the
    // exact pixel sizes this display wants (16 DIPs for the title bar, 32 for
    // the taskbar and Alt-Tab), so Windows picks the sharpest image and no
    // image decoder ships in the binary.
    let icon_size = |dips: f64| {
        let px = (dips * scale).round() as u32;
        Some(tao::dpi::PhysicalSize::new(px, px))
    };
    window.set_window_icon(Icon::from_resource(ICON_RESOURCE, icon_size(16.0)).ok());
    window.set_taskbar_icon(Icon::from_resource(ICON_RESOURCE, icon_size(32.0)).ok());

    trace.borrow_mut().mark("window_created");

    let mut renderer = Renderer::new(window.hwnd(), (physical.width, physical.height))
        .expect("Failed to initialise Direct2D");
    trace.borrow_mut().mark("d2d_ready");

    let (blocks, anchors) = parse(source);
    renderer.blocks = blocks;
    renderer.anchors = anchors;
    trace.borrow_mut().mark("markdown_parsed");

    renderer
        .relayout((logical.width, logical.height))
        .expect("Failed to lay out document");
    trace.borrow_mut().mark("laid_out");

    // Paint before the window is shown so it never appears empty.
    let _ = renderer.paint();
    window.set_visible(true);
    {
        let mut trace = trace.borrow_mut();
        trace.mark("window_shown");
        trace.flush();
    }

    let mut autoscroll: Option<Autoscroll> = None;
    let mut modifiers = ModifiersState::empty();
    // Link under the left button when it went down; it opens only if the
    // button is released over the same link.
    let mut pressed_link: Option<String> = None;
    // Cursor shape currently shown for hover (arrow, hand over links, I-beam
    // over text), so it is only set on change.
    let mut hover_cursor = CursorIcon::Default;
    // Where the left button went down, to tell a click from a drag-select.
    let mut press_point = (0.0_f32, 0.0_f32);

    event_loop.run(move |event, _, control_flow| {
        // While autoscrolling, wake on a timer to step the scroll; otherwise
        // sleep until input arrives.
        *control_flow = match &autoscroll {
            Some(active) => ControlFlow::WaitUntil(active.last_step + AUTOSCROLL_FRAME),
            None => ControlFlow::Wait,
        };

        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                if let Some(active) = autoscroll.as_mut() {
                    let now = Instant::now();
                    let elapsed = now.duration_since(active.last_step).as_secs_f32();
                    active.last_step = now;
                    if renderer.scroll_by(active.velocity(renderer.cursor.1) * elapsed) {
                        let _ = renderer.paint();
                    }
                    *control_flow = ControlFlow::WaitUntil(now + AUTOSCROLL_FRAME);
                }
            }

            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                // Exit directly; see the note in main.rs on tao's exit handling.
                std::process::exit(0);
            }

            Event::WindowEvent { event: WindowEvent::Resized(size), .. } => {
                if renderer.resize((size.width, size.height)).is_ok() {
                    let logical = size.to_logical::<f32>(window.scale_factor());
                    let _ = renderer.relayout((logical.width, logical.height));
                    let _ = renderer.paint();
                }
            }

            Event::WindowEvent { event: WindowEvent::CursorMoved { position, .. }, .. } => {
                let p = position.to_logical::<f32>(window.scale_factor());
                renderer.cursor = (p.x, p.y);
                if let Some(active) = &autoscroll {
                    window.set_cursor_icon(active.cursor_icon(p.y));
                }
                let moved = renderer.drag_to(p.y);
                let hover_changed = renderer.update_hover();
                if moved || hover_changed {
                    let _ = renderer.paint();
                }
                if renderer.selecting {
                    let (dx, dy) = (p.x - press_point.0, p.y - press_point.1);
                    if (dx * dx + dy * dy).sqrt() > DRAG_THRESHOLD {
                        // Moving off the press point turns a link click into a selection.
                        pressed_link = None;
                    }
                    if pressed_link.is_none() {
                        // Dragging past the top or bottom edge scrolls the document.
                        let edge = if p.y < 0.0 {
                            p.y
                        } else if p.y > renderer.view.1 {
                            p.y - renderer.view.1
                        } else {
                            0.0
                        };
                        if edge != 0.0 {
                            renderer.scroll_by(edge);
                        }
                        if let Some(caret) = renderer.caret_at(renderer.cursor) {
                            if let Some(selection) = renderer.selection.as_mut() {
                                selection.focus = caret;
                            }
                        }
                        let _ = renderer.paint();
                    }
                }
                if autoscroll.is_none() && renderer.drag_grab.is_none() {
                    let cursor = renderer.cursor;
                    let icon = if renderer.selecting && pressed_link.is_none() {
                        CursorIcon::Text
                    } else if renderer.link_at(cursor).is_some() {
                        CursorIcon::Hand
                    } else if !renderer.in_gutter(cursor.0)
                        && renderer.bar_hit(cursor).is_none()
                        && renderer.hit_text(cursor).is_some()
                    {
                        CursorIcon::Text
                    } else {
                        CursorIcon::Default
                    };
                    if icon != hover_cursor {
                        hover_cursor = icon;
                        window.set_cursor_icon(icon);
                    }
                }
            }

            Event::WindowEvent { event: WindowEvent::ModifiersChanged(state), .. } => {
                modifiers = state;
            }

            Event::WindowEvent { event: WindowEvent::CursorLeft { .. }, .. } => {
                // During autoscroll, keep the last in-window position so scrolling
                // continues at the speed it had at the edge, as in browsers.
                if autoscroll.is_some() || renderer.selecting {
                    return;
                }
                renderer.cursor = (-1.0, -1.0);
                if renderer.update_hover() {
                    let _ = renderer.paint();
                }
            }

            Event::WindowEvent {
                event: WindowEvent::MouseInput { state, button: MouseButton::Left, .. },
                ..
            } => match state {
                ElementState::Pressed => {
                    if stop_autoscroll(&mut autoscroll, &window, control_flow) {
                        // The click only cancels autoscroll.
                    } else if let Some(hit) = renderer.bar_hit(renderer.cursor) {
                        if hit != BarHit::Inside {
                            renderer.step_search(hit == BarHit::Next);
                            let _ = renderer.paint();
                        }
                    } else if renderer.in_gutter(renderer.cursor.0) {
                        let y = renderer.cursor.1;
                        if renderer.press_gutter(y) {
                            let _ = renderer.paint();
                        }
                    } else {
                        pressed_link = renderer.link_at(renderer.cursor).map(str::to_owned);
                        press_point = renderer.cursor;
                        // A fresh press clears any old selection and anchors a new one.
                        let had_selection = renderer.selection.is_some_and(|s| !s.is_empty());
                        renderer.selection = renderer
                            .caret_at(renderer.cursor)
                            .map(|caret| Selection { anchor: caret, focus: caret });
                        renderer.selecting = true;
                        if had_selection {
                            let _ = renderer.paint();
                        }
                    }
                }
                ElementState::Released => {
                    renderer.selecting = false;
                    if renderer.drag_grab.take().is_some() {
                        renderer.update_hover();
                        let _ = renderer.paint();
                    }
                    let Some(dest) = pressed_link.take() else {
                        return;
                    };
                    if renderer.link_at(renderer.cursor) != Some(dest.as_str()) {
                        return;
                    }
                    match classify_link(&dest, base_dir.as_deref()) {
                        LinkAction::Anchor(fragment) => {
                            if renderer.jump_to_anchor(&fragment) {
                                let _ = renderer.paint();
                            }
                        }
                        LinkAction::Browser(url) => open_in_browser(&url),
                        LinkAction::Document(file) => open_document(&file),
                        LinkAction::Ignore => {}
                    }
                }
                _ => {}
            },

            Event::WindowEvent {
                event: WindowEvent::MouseInput { state, button: MouseButton::Middle, .. },
                ..
            } => match state {
                ElementState::Pressed => {
                    if !stop_autoscroll(&mut autoscroll, &window, control_flow) {
                        let active = Autoscroll::new(renderer.cursor.1);
                        window.set_cursor_icon(active.cursor_icon(renderer.cursor.1));
                        hover_cursor = CursorIcon::Default;
                        *control_flow = ControlFlow::WaitUntil(active.last_step + AUTOSCROLL_FRAME);
                        autoscroll = Some(active);
                    }
                }
                ElementState::Released => {
                    // A quick click leaves autoscroll running; a hold ends on release.
                    let held = autoscroll
                        .as_ref()
                        .is_some_and(|active| active.pressed_at.elapsed() >= AUTOSCROLL_HOLD);
                    if held {
                        stop_autoscroll(&mut autoscroll, &window, control_flow);
                    }
                }
                _ => {}
            },

            Event::WindowEvent {
                event: WindowEvent::MouseInput { state: ElementState::Pressed, .. },
                ..
            } => {
                stop_autoscroll(&mut autoscroll, &window, control_flow);
            }

            Event::WindowEvent { event: WindowEvent::MouseWheel { delta, .. }, .. } => {
                stop_autoscroll(&mut autoscroll, &window, control_flow);
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * BODY_LINE * 3.0,
                    MouseScrollDelta::PixelDelta(p) => -p.y as f32,
                    _ => 0.0,
                };
                if renderer.scroll_by(dy) {
                    let _ = renderer.paint();
                }
            }

            Event::WindowEvent { event: WindowEvent::KeyboardInput { event, .. }, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                // A key press only cancels autoscroll; Escape here must not close.
                if stop_autoscroll(&mut autoscroll, &window, control_flow) {
                    return;
                }

                if modifiers.control_key() && event.physical_key == KeyCode::KeyF {
                    renderer.open_search();
                    let _ = renderer.paint();
                    return;
                }
                if modifiers.control_key() && event.physical_key == KeyCode::KeyC {
                    let text = renderer.selected_text();
                    if !text.is_empty() {
                        let _ = copy_to_clipboard(HWND(window.hwnd() as *mut core::ffi::c_void), &text);
                    }
                    return;
                }
                if modifiers.control_key() && event.physical_key == KeyCode::KeyA {
                    renderer.select_all();
                    let _ = renderer.paint();
                    return;
                }
                if renderer.search.is_some() {
                    match event.physical_key {
                        KeyCode::Escape => {
                            renderer.search = None;
                            let _ = renderer.paint();
                            return;
                        }
                        KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::F3 => {
                            renderer.step_search(!modifiers.shift_key());
                            let _ = renderer.paint();
                            return;
                        }
                        KeyCode::Backspace => {
                            renderer.edit_query(|query| {
                                query.pop();
                            });
                            let _ = renderer.paint();
                            return;
                        }
                        _ => {}
                    }
                    // Printable input goes to the query, including Space, which
                    // otherwise pages down.
                    if !modifiers.control_key() && !modifiers.alt_key() {
                        let typed: String = event
                            .text
                            .unwrap_or_default()
                            .chars()
                            .filter(|c| !c.is_control())
                            .collect();
                        if !typed.is_empty() {
                            renderer.edit_query(|query| query.push_str(&typed));
                            let _ = renderer.paint();
                            return;
                        }
                    }
                }

                let page = renderer.view.1 * 0.9;
                let dy = match event.physical_key {
                    KeyCode::ArrowDown => BODY_LINE * 3.0,
                    KeyCode::ArrowUp => -BODY_LINE * 3.0,
                    KeyCode::PageDown | KeyCode::Space => page,
                    KeyCode::PageUp => -page,
                    KeyCode::Home => -renderer.content_height,
                    KeyCode::End => renderer.content_height,
                    KeyCode::Escape => std::process::exit(0),
                    _ => 0.0,
                };
                if dy != 0.0 && renderer.scroll_by(dy) {
                    let _ = renderer.paint();
                }
            }

            Event::RedrawRequested(_) => {
                let _ = renderer.paint();
            }

            _ => {}
        }
    });
}
