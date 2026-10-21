//! Shared CPU rasteriser, text layout, and event loop for every platform.
//!
//! This is the single rendering core behind both backends. It owns the whole
//! visual pipeline — FreeType glyph loading and caching, word-wrapped text
//! layout, hit-testing, search, selection, and the alpha-blended software
//! compositor — and produces a 32-bit `0xAARRGGBB` frame buffer that a thin
//! platform shell blits to its window.
//!
//! Everything here is compiled on all targets. The two things that genuinely
//! differ per platform are injected through small traits:
//!
//!   * [`FontSource`] resolves the four face file paths. Linux resolves them
//!     through Fontconfig; Windows reads them from the font registry. The
//!     FreeType loading itself (identical everywhere) stays here.
//!   * [`Host`] is the window the event loop drives: it reports size and scale,
//!     shows itself, sets the cursor, runs a timer, offers a text selection to
//!     the system, and presents a frame. `x11.rs` and `win.rs` each implement
//!     it over their native window, translating native events into the shared
//!     [`Ev`] enum this module defines.
//!
//! The platform-independent markdown parse, block model, link classification,
//! and search case-folding live in [`crate::doc`] and are shared verbatim.
//!
//! Because DirectWrite's `IDWriteTextLayout` is not used on any platform now,
//! this file carries its own text layout ([`Layout`]): word-wrapped glyph runs
//! that measure their height, hit-test a point to a UTF-16 offset, and return
//! the rectangles covering a UTF-16 range. All offsets stay in UTF-16 units so
//! the shared search and selection maths are identical to the historical
//! DirectWrite behaviour.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use freetype_sys as ft;

use crate::doc::{
    BAR_ARROW_W, BAR_H, BAR_MARGIN, BAR_PAD, BAR_W, BG, BLOCK_GAP, BODY_LINE, BODY_SIZE, BORDER,
    BORDER_SOFT, BarHit, Block, CELL_PAD_X, CELL_PAD_Y, CODE_LINE, CODE_PAD, CODE_SIZE, Caret,
    DRAG_THRESHOLD, FG, FG_MUTED, FG_STRONG, Flavor, HEADING_GAP_ABOVE, ITEM_GAP, ITEM_LINE, Ink,
    LINK, LinkAction, MATCH, MATCH_ALPHA, MATCH_CURRENT_ALPHA, MAX_COLUMN, PAD_X, PAD_Y, QUOTE_BAR,
    QUOTE_PAD, RULE_GAP, SCROLLBAR_W, SELECTION, SELECTION_ALPHA, SURFACE, Selection, Source, Style,
    THUMB, THUMB_HOVER, THUMB_MIN, Text, classify_link, fold, percent_decode,
};

// ---------------------------------------------------------------------------
// Shared input vocabulary — both shells translate native events into these.
// ---------------------------------------------------------------------------

/// Mouse buttons the app distinguishes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Button {
    Left,
    Middle,
    Other,
}

/// Cursor shapes the app shows over its client area.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cursor {
    Arrow,
    Hand,
    IBeam,
    SizeNs,
}

/// Input and window events, translated from the platform's native events.
pub(crate) enum Ev {
    Close,
    /// New client size in physical pixels.
    Resized(u32, u32),
    /// Cursor position in client DIPs; may lie outside the window while a
    /// button is held, because both platforms keep delivering motion during a
    /// drag (an X implicit grab, or a Win32 mouse capture).
    Moved(f32, f32),
    /// The cursor left the client area.
    Left,
    Button { button: Button, down: bool, at: (f32, f32) },
    /// Wheel notches, positive away from the user.
    Wheel(f32),
    /// A key press, as a Win32-style virtual-key code.
    Key { vk: u16, ctrl: bool, shift: bool },
    /// A typed character, after keyboard-layout translation.
    Char(char),
    Timer,
    Paint,
}

// ---------------------------------------------------------------------------
// Platform injection points
// ---------------------------------------------------------------------------

/// Resolves the four display faces to a font file path and face index.
///
/// The only per-platform part of font handling: Linux resolves through
/// Fontconfig, Windows through the font registry. FreeType then loads the
/// returned files identically on both.
pub(crate) trait FontSource {
    /// Returns the file path and face index for `kind`, or `None` if the
    /// platform cannot resolve it.
    ///
    /// Args:
    ///     kind: Which display face to resolve.
    ///
    /// Returns:
    ///     `(path, face_index)`, or `None`.
    fn resolve(&self, kind: FaceKind) -> Option<(String, u32)>;
}

/// The window the event loop drives. Both shells implement it over their
/// native window; the [`drive`] loop only ever touches the window through
/// these methods.
pub(crate) trait Host {
    /// Physical pixels per DIP for the monitor the window is on.
    fn scale(&self) -> f32;
    /// Client area size in physical pixels.
    fn client_size(&self) -> (u32, u32);
    /// Maps/shows the window so it becomes visible.
    fn show(&self);
    /// Sets the cursor shown over the client area.
    fn set_cursor(&self, cursor: Cursor);
    /// Starts a repeating timer delivering [`Ev::Timer`] every `ms`.
    fn start_timer(&self, ms: u32);
    /// Stops the repeating timer.
    fn stop_timer(&self);
    /// Offers `text` to the system clipboard/selection.
    fn set_selection(&self, text: &str);
    /// Uploads a `0xAARRGGBB` frame of `w`×`h` device pixels to the window.
    fn present(&self, pixels: &[u32], w: u32, h: u32);

    /// Runs the platform event loop until the window closes, delivering every
    /// translated [`Ev`] to `handler` with a borrow of the host.
    ///
    /// The host is consumed and leaked for the program's lifetime so the
    /// renderer's FreeType/glyph-cache state is never torn down mid-exit; the
    /// process exits on [`Ev::Close`] instead.
    ///
    /// Args:
    ///     handler: Called once per event on the UI thread.
    ///
    /// Returns:
    ///     Never; the process exits when the window closes.
    fn run_loop(self, handler: impl FnMut(&Self, Ev) + 'static) -> !
    where
        Self: Sized;
}

// ---------------------------------------------------------------------------
// Geometry helper
// ---------------------------------------------------------------------------

/// An axis-aligned rectangle in float coordinates, the renderer's own stand-in
/// for the old `D2D_RECT_F`.
#[derive(Clone, Copy)]
struct RectF {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

// ---------------------------------------------------------------------------
// Fonts and glyph cache
// ---------------------------------------------------------------------------

/// Which resolved face a run is drawn with.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FaceKind {
    Sans,
    SansBold,
    SansItalic,
    Mono,
}

/// A rasterised glyph: an 8-bit coverage bitmap plus placement metrics, in
/// pixels, cached across frames.
struct Glyph {
    /// Coverage, row-major, `width * rows` bytes.
    coverage: Vec<u8>,
    width: i32,
    rows: i32,
    /// Pen-relative offset of the bitmap's top-left corner.
    left: i32,
    top: i32,
    /// Horizontal advance after the glyph.
    advance: f32,
}

/// Owns the FreeType library and the four resolved faces, and caches glyphs.
struct Fonts {
    library: ft::FT_Library,
    faces: HashMap<FaceKind, ft::FT_Face>,
    /// `(face, glyph_index, px_size_times_64)` -> rasterised glyph.
    cache: HashMap<(FaceKind, u32, u32), Glyph>,
    /// Pixel size the most recent `set_size` applied, per face, to skip
    /// redundant `FT_Set_Pixel_Sizes` calls.
    sized: HashMap<FaceKind, u32>,
}

impl Fonts {
    /// Initialises FreeType and loads the four faces resolved by `source`.
    ///
    /// The path discovery is the caller's job (Fontconfig on Linux, the font
    /// registry on Windows); the FreeType loading below is identical on every
    /// platform.
    ///
    /// Args:
    ///     source: Resolves each [`FaceKind`] to a font file and face index.
    ///
    /// Returns:
    ///     The font set, or a message describing what failed.
    fn new(source: &dyn FontSource) -> Result<Self, String> {
        // SAFETY: FT_Init_FreeType writes a library handle into `library`.
        let mut library: ft::FT_Library = std::ptr::null_mut();
        let rc = unsafe { ft::FT_Init_FreeType(&mut library) };
        if rc != 0 || library.is_null() {
            return Err(format!("FT_Init_FreeType failed ({rc})"));
        }

        let mut faces = HashMap::new();
        for kind in [FaceKind::Sans, FaceKind::SansBold, FaceKind::SansItalic, FaceKind::Mono] {
            let (path, index) = match source.resolve(kind) {
                Some(found) => found,
                None => {
                    unsafe { ft::FT_Done_FreeType(library) };
                    return Err(String::from("could not resolve a required font"));
                }
            };
            let cpath = CString::new(path.as_bytes()).map_err(|_| "font path had a NUL")?;
            let mut face: ft::FT_Face = std::ptr::null_mut();
            // SAFETY: `cpath` outlives the call; `face` receives the handle.
            let rc = unsafe {
                ft::FT_New_Face(library, cpath.as_ptr(), index as ft::FT_Long, &mut face)
            };
            if rc != 0 || face.is_null() {
                unsafe { ft::FT_Done_FreeType(library) };
                return Err(format!("FT_New_Face failed for {path} ({rc})"));
            }
            faces.insert(kind, face);
        }

        Ok(Fonts { library, faces, cache: HashMap::new(), sized: HashMap::new() })
    }

    /// Returns the face handle for a kind, falling back to Sans.
    fn face(&self, kind: FaceKind) -> ft::FT_Face {
        *self.faces.get(&kind).or_else(|| self.faces.get(&FaceKind::Sans)).unwrap()
    }

    /// Sets a face's pixel size, skipping the call when already current.
    ///
    /// Args:
    ///     kind: Which face to size.
    ///     px: Pixel size, rounded to a whole pixel.
    fn set_size(&mut self, kind: FaceKind, px: u32) {
        if self.sized.get(&kind) == Some(&px) {
            return;
        }
        let face = self.face(kind);
        // SAFETY: `face` is a live face created in `new`.
        unsafe {
            ft::FT_Set_Pixel_Sizes(face, 0, px.max(1));
        }
        self.sized.insert(kind, px);
    }

    /// Looks up the glyph index for a character in a face.
    fn glyph_index(&self, kind: FaceKind, ch: u32) -> u32 {
        let face = self.face(kind);
        // SAFETY: `face` is a live face.
        unsafe { ft::FT_Get_Char_Index(face, ch as ft::FT_ULong) }
    }

    /// Rasterises and caches one glyph, returning a stable reference to it.
    ///
    /// Args:
    ///     kind: Face to render from.
    ///     px: Pixel size.
    ///     index: Glyph index within the face.
    ///
    /// Returns:
    ///     The cached glyph, or `None` if FreeType could not load it.
    fn glyph(&mut self, kind: FaceKind, px: u32, index: u32) -> Option<&Glyph> {
        let key = (kind, index, px);
        if !self.cache.contains_key(&key) {
            self.set_size(kind, px);
            let face = self.face(kind);
            // SAFETY: a live, sized face; FT_LOAD_RENDER fills the glyph slot
            // and its bitmap, which are read immediately below.
            unsafe {
                if ft::FT_Load_Glyph(face, index, ft::FT_LOAD_RENDER) != 0 {
                    return None;
                }
                let slot = &*(*face).glyph;
                let bitmap = &slot.bitmap;
                let width = bitmap.width;
                let rows = bitmap.rows;
                let pitch = bitmap.pitch;
                let mut coverage = vec![0u8; (width.max(0) * rows.max(0)) as usize];
                if !bitmap.buffer.is_null() && width > 0 && rows > 0 {
                    for row in 0..rows {
                        let src = bitmap.buffer.offset((row * pitch) as isize);
                        let dst = row * width;
                        std::ptr::copy_nonoverlapping(
                            src,
                            coverage.as_mut_ptr().add(dst as usize),
                            width as usize,
                        );
                    }
                }
                // 26.6 fixed-point advance to pixels.
                let advance = (slot.advance.x as f32) / 64.0;
                let glyph = Glyph {
                    coverage,
                    width,
                    rows,
                    left: slot.bitmap_left,
                    top: slot.bitmap_top,
                    advance,
                };
                self.cache.insert(key, glyph);
            }
        }
        self.cache.get(&key)
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        // SAFETY: handles created in `new`; dropped once, at process teardown
        // paths that are not the leaked window (e.g. a construction failure).
        unsafe {
            for face in self.faces.values() {
                ft::FT_Done_Face(*face);
            }
            if !self.library.is_null() {
                ft::FT_Done_FreeType(self.library);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Text layout (replaces IDWriteTextLayout)
// ---------------------------------------------------------------------------

/// One laid-out glyph, recorded for drawing and hit-testing.
#[derive(Clone)]
struct PlacedGlyph {
    kind: FaceKind,
    px: u32,
    index: u32,
    ink: Ink,
    /// UTF-16 offset of the source character (first unit of a surrogate pair).
    source: u32,
    /// Number of UTF-16 units this glyph consumed (1, or 2 for astral chars).
    units: u32,
    /// Pen x of the glyph's origin, relative to the layout's left edge.
    x: f32,
    advance: f32,
    /// Index of the line this glyph sits on.
    line: usize,
}

/// A single wrapped line's vertical band.
#[derive(Clone)]
struct LineBox {
    top: f32,
    bottom: f32,
    baseline: f32,
    /// UTF-16 offset where this line begins; the caret for a glyph-less line.
    start: u32,
}

/// A word-wrapped run of styled text, the renderer's `IDWriteTextLayout`.
#[derive(Clone)]
struct Layout {
    glyphs: Vec<PlacedGlyph>,
    lines: Vec<LineBox>,
    /// UTF-16 length of the source text, so a caret can land after the end.
    text_len: u32,
    height: f32,
}

impl Layout {
    /// Rectangles covering a UTF-16 range, offset to document coordinates.
    ///
    /// Mirrors `IDWriteTextLayout::HitTestTextRange`: one rectangle per line
    /// fragment the range touches.
    ///
    /// Args:
    ///     start: First UTF-16 position of the range.
    ///     len: Length of the range in UTF-16 units.
    ///     origin: Document position of the layout's top-left corner.
    ///
    /// Returns:
    ///     One rectangle per covered line fragment.
    fn range_rects(&self, start: u32, len: u32, origin: (f32, f32)) -> Vec<RectF> {
        let (ox, oy) = origin;
        let end = start + len;
        let mut rects: Vec<RectF> = Vec::new();
        // Accumulate the horizontal extent per line the range covers.
        let mut per_line: HashMap<usize, (f32, f32)> = HashMap::new();
        for glyph in &self.glyphs {
            let g_start = glyph.source;
            let g_end = glyph.source + glyph.units;
            // Intersect the glyph's unit span with the requested range.
            if g_end <= start || g_start >= end {
                continue;
            }
            let left = glyph.x;
            let right = glyph.x + glyph.advance;
            let entry = per_line.entry(glyph.line).or_insert((f32::MAX, f32::MIN));
            entry.0 = entry.0.min(left);
            entry.1 = entry.1.max(right);
        }
        let mut lines: Vec<usize> = per_line.keys().copied().collect();
        lines.sort_unstable();
        for line_index in lines {
            let (left, right) = per_line[&line_index];
            let line = &self.lines[line_index];
            rects.push(RectF {
                left: ox + left,
                top: oy + line.top,
                right: ox + right,
                bottom: oy + line.bottom,
            });
        }
        rects
    }

    /// The UTF-16 offset nearest a point within the layout, and whether the
    /// point fell inside any glyph box (as `HitTestPoint`'s `isInside`).
    ///
    /// Args:
    ///     x: Point x relative to the layout's left edge.
    ///     y: Point y relative to the layout's top edge.
    ///
    /// Returns:
    ///     `(offset, inside)` where `offset` is a UTF-16 caret position.
    fn hit_test(&self, x: f32, y: f32) -> (u32, bool) {
        if self.lines.is_empty() {
            return (0, false);
        }
        // Pick the line whose vertical band contains y, else the nearest.
        let mut line_index = 0usize;
        let mut best_dy = f32::MAX;
        let mut inside_line = false;
        for (index, line) in self.lines.iter().enumerate() {
            if y >= line.top && y <= line.bottom {
                line_index = index;
                inside_line = true;
                break;
            }
            let dy = (line.top - y).max(y - line.bottom).max(0.0);
            if dy < best_dy {
                best_dy = dy;
                line_index = index;
            }
        }

        // Among this line's glyphs, choose the caret edge nearest x.
        let mut best_offset = self.text_len;
        let mut best_dx = f32::MAX;
        let mut inside_glyph = false;
        let mut saw_glyph = false;
        for glyph in self.glyphs.iter().filter(|g| g.line == line_index) {
            saw_glyph = true;
            let left = glyph.x;
            let right = glyph.x + glyph.advance;
            let mid = (left + right) / 2.0;
            // Leading edge of this glyph.
            let dx_lead = (x - left).abs();
            if dx_lead < best_dx {
                best_dx = dx_lead;
                best_offset = glyph.source;
            }
            // Trailing edge.
            let dx_trail = (x - right).abs();
            if dx_trail < best_dx {
                best_dx = dx_trail;
                best_offset = glyph.source + glyph.units;
            }
            if x >= left && x <= right {
                inside_glyph = true;
                best_offset = if x < mid { glyph.source } else { glyph.source + glyph.units };
            }
        }
        if !saw_glyph {
            // Empty line: caret sits at the line's own start offset.
            best_offset = self.lines[line_index].start;
        }
        (best_offset.min(self.text_len), inside_line && inside_glyph)
    }
}

// ---------------------------------------------------------------------------
// Display list
// ---------------------------------------------------------------------------

enum Draw {
    Text { layout: Layout, x: f32, y: f32, ink: Ink, source: Source },
    /// `radius` of 0 draws square corners.
    Rect { rect: RectF, ink: Ink, radius: f32 },
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
    item: usize,
    rects: Vec<RectF>,
}

/// Ctrl+F state: the query being typed and where it occurs.
struct Search {
    query: String,
    matches: Vec<Match>,
    current: usize,
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// The software renderer: owns the fonts, the display list, and the frame.
pub(crate) struct Renderer {
    fonts: Fonts,
    /// Frame buffer in `0xAARRGGBB`, `pixels.len() == width * height`.
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    blocks: Vec<Block>,
    items: Vec<Item>,
    anchors: Vec<(String, usize)>,
    block_tops: Vec<f32>,
    base_dir: Option<PathBuf>,
    search: Option<Search>,
    selection: Option<Selection>,
    selecting: bool,
    content_height: f32,
    view: (f32, f32),
    scroll: f32,
    /// Device pixels per DIP for the last laid-out frame.
    scale: f32,
    cursor: (f32, f32),
    drag_grab: Option<f32>,
    hover_thumb: bool,
}

/// Vertical placement of the scrollbar thumb, in DIPs.
struct Thumb {
    top: f32,
    height: f32,
}

// Autoscroll tuning, unchanged from the historical backends.
const AUTOSCROLL_DEAD_ZONE: f32 = 12.0;
const AUTOSCROLL_GAIN: f32 = 8.0;
const AUTOSCROLL_FRAME: Duration = Duration::from_millis(16);
const AUTOSCROLL_HOLD: Duration = Duration::from_millis(300);

/// Middle-click autoscroll in progress: a middle click anchors at the cursor;
/// moving away scrolls at a speed proportional to the distance past a dead
/// zone. Any button, key, or wheel input ends it, as does releasing the middle
/// button after holding it.
struct Autoscroll {
    anchor_y: f32,
    pressed_at: Instant,
    last_step: Instant,
}

impl Autoscroll {
    fn new(anchor_y: f32) -> Self {
        let now = Instant::now();
        Self { anchor_y, pressed_at: now, last_step: now }
    }

    fn velocity(&self, cursor_y: f32) -> f32 {
        let offset = cursor_y - self.anchor_y;
        let past = (offset.abs() - AUTOSCROLL_DEAD_ZONE).max(0.0);
        offset.signum() * past * AUTOSCROLL_GAIN
    }
}

/// Resolves an [`Ink`] to its `0xAARRGGBB` colour, applying translucency for
/// the highlight slots.
fn ink_color(ink: Ink) -> u32 {
    match ink {
        Ink::Background => opaque(BG),
        Ink::Text => opaque(FG),
        Ink::Strong => opaque(FG_STRONG),
        Ink::Muted => opaque(FG_MUTED),
        Ink::Surface => opaque(SURFACE),
        Ink::Border => opaque(BORDER),
        Ink::BorderSoft => opaque(BORDER_SOFT),
        Ink::Link => opaque(LINK),
        Ink::Thumb => opaque(THUMB),
        Ink::ThumbHover => opaque(THUMB_HOVER),
        Ink::Match => with_alpha(MATCH, MATCH_ALPHA),
        Ink::MatchCurrent => with_alpha(MATCH, MATCH_CURRENT_ALPHA),
        Ink::Selection => with_alpha(SELECTION, SELECTION_ALPHA),
    }
}

/// Packs an opaque `0xRRGGBB` as `0xFFRRGGBB`.
fn opaque(rgb: u32) -> u32 {
    0xFF00_0000 | (rgb & 0x00FF_FFFF)
}

/// Packs `0xRRGGBB` with a float alpha into `0xAARRGGBB`.
fn with_alpha(rgb: u32, alpha: f32) -> u32 {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    (a << 24) | (rgb & 0x00FF_FFFF)
}

impl Renderer {
    /// Creates the renderer and its fonts, with no document loaded yet.
    ///
    /// Args:
    ///     source: Platform font resolver, used to find the four face files.
    ///
    /// Returns:
    ///     The renderer, or a message describing what failed.
    pub(crate) fn new(source: &dyn FontSource) -> Result<Self, String> {
        let fonts = Fonts::new(source)?;
        Ok(Renderer {
            fonts,
            pixels: Vec::new(),
            width: 0,
            height: 0,
            blocks: Vec::new(),
            items: Vec::new(),
            anchors: Vec::new(),
            block_tops: Vec::new(),
            base_dir: None,
            search: None,
            selection: None,
            selecting: false,
            content_height: 0.0,
            view: (0.0, 0.0),
            scroll: 0.0,
            scale: 1.0,
            cursor: (-1.0, -1.0),
            drag_grab: None,
            hover_thumb: false,
        })
    }

    /// Installs the parsed document before the first layout.
    ///
    /// Args:
    ///     blocks: Parsed block list.
    ///     anchors: Heading anchor slugs with their block indices.
    ///     base_dir: Folder of the open file, for resolving relative links.
    pub(crate) fn set_document(
        &mut self,
        blocks: Vec<Block>,
        anchors: Vec<(String, usize)>,
        base_dir: Option<PathBuf>,
    ) {
        self.blocks = blocks;
        self.anchors = anchors;
        self.base_dir = base_dir;
    }

    /// The current frame buffer and its device dimensions, for the window to
    /// blit.
    ///
    /// Returns:
    ///     `(pixels, width, height)` in device pixels and `0xAARRGGBB` words.
    pub(crate) fn frame(&self) -> (&[u32], u32, u32) {
        (&self.pixels, self.width, self.height)
    }

    // --- Style mapping -----------------------------------------------------

    /// Maps a flavour to its base face, pixel size (in DIPs), and line height.
    ///
    /// Args:
    ///     flavor: The block flavour.
    ///
    /// Returns:
    ///     `(face, size_dip, line_height_dip)`.
    fn style_for(flavor: Flavor) -> (FaceKind, f32, f32) {
        match flavor {
            Flavor::Heading(n) => {
                let size = [32.0_f32, 24.0, 20.0, 16.0, 16.0, 16.0][(n as usize - 1).min(5)];
                (FaceKind::SansBold, size, size * 1.25)
            }
            Flavor::Code => (FaceKind::Mono, CODE_SIZE, CODE_LINE),
            Flavor::Item => (FaceKind::Sans, BODY_SIZE, ITEM_LINE),
            Flavor::Body => (FaceKind::Sans, BODY_SIZE, BODY_LINE),
        }
    }

    /// Chooses the face and ink for an inline-styled unit.
    ///
    /// Args:
    ///     base: The run's base face.
    ///     base_ink: The run's base ink.
    ///     style: The inline style over this unit, if any.
    ///
    /// Returns:
    ///     `(face, size_override, ink)`; `size_override` is `Some` for inline
    ///     code, which shrinks to `CODE_SIZE`.
    fn inline_style(
        base: FaceKind,
        base_ink: Ink,
        style: Option<Style>,
    ) -> (FaceKind, Option<f32>, Ink) {
        match style {
            Some(Style::Strong) => (FaceKind::SansBold, None, Ink::Strong),
            Some(Style::Emphasis) => (FaceKind::SansItalic, None, base_ink),
            Some(Style::Code) => (FaceKind::Mono, Some(CODE_SIZE), base_ink),
            Some(Style::Link) => (base, None, Ink::Link),
            None => (base, None, base_ink),
        }
    }

    /// Finds the innermost inline style covering a UTF-16 offset.
    fn style_at(text: &Text, offset: u32) -> Option<Style> {
        text.spans
            .iter()
            .rev()
            .find(|s| offset >= s.start && offset < s.start + s.len)
            .map(|s| s.style)
    }

    // --- Text layout -------------------------------------------------------

    /// Lays out a run of styled text, word-wrapping at `width` DIPs.
    ///
    /// Mirrors `DWRITE_WORD_WRAPPING_WRAP`: a word that would overflow the wrap
    /// width starts a new line, but a single word wider than the line is left
    /// to overflow rather than split. Line height is the uniform doc constant,
    /// as DirectWrite's uniform line spacing was.
    ///
    /// Args:
    ///     text: The source run (UTF-16 with inline spans).
    ///     base: Base face for the run.
    ///     base_ink: Base ink for the run.
    ///     base_size: Base pixel size in DIPs.
    ///     line_height: Line box height in DIPs.
    ///     width: Wrap width in DIPs.
    ///
    /// Returns:
    ///     The laid-out run, measured in DIPs.
    fn build_layout(
        &mut self,
        text: &Text,
        base: FaceKind,
        base_ink: Ink,
        base_size: f32,
        line_height: f32,
        width: f32,
    ) -> Layout {
        let scale = self.scale;
        let units = &text.utf16;
        let text_len = units.len() as u32;

        // Decode UTF-16 into (char, source_offset, unit_count), handling
        // surrogate pairs and explicit hard-break newlines.
        let mut chars: Vec<(char, u32, u32)> = Vec::with_capacity(units.len());
        let mut i = 0usize;
        while i < units.len() {
            let unit = units[i];
            if (0xD800..=0xDBFF).contains(&unit) && i + 1 < units.len() {
                let low = units[i + 1];
                if (0xDC00..=0xDFFF).contains(&low) {
                    if let Some(Ok(c)) = char::decode_utf16([unit, low]).next() {
                        chars.push((c, i as u32, 2));
                        i += 2;
                        continue;
                    }
                }
            }
            let c = char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}');
            chars.push((c, i as u32, 1));
            i += 1;
        }

        let mut glyphs: Vec<PlacedGlyph> = Vec::with_capacity(chars.len());
        let mut lines: Vec<LineBox> = Vec::new();
        let mut line_index = 0usize;
        let mut pen_x = 0.0f32;
        // Glyph index of the last breakable point (just after a space), used to
        // rewind the wrap to a word boundary.
        let mut last_break: Option<usize> = None;
        let mut line_start = 0u32;
        let line_top = |idx: usize| idx as f32 * line_height;

        for (ch, source, unit_count) in chars {
            if ch == '\n' {
                lines.push(LineBox {
                    top: line_top(line_index),
                    bottom: line_top(line_index) + line_height,
                    baseline: line_top(line_index) + line_height * 0.8,
                    start: line_start,
                });
                line_index += 1;
                line_start = source + unit_count;
                pen_x = 0.0;
                last_break = None;
                continue;
            }

            let (face, size_override, ink) =
                Self::inline_style(base, base_ink, Self::style_at(text, source));
            let px = ((size_override.unwrap_or(base_size)) * scale).round().max(1.0) as u32;
            let index = self.fonts.glyph_index(face, u32::from(ch));
            let advance = self
                .fonts
                .glyph(face, px, index)
                .map(|g| g.advance / scale)
                .unwrap_or(base_size * 0.5);

            // Wrap: if adding this glyph overflows, break at the last space.
            if pen_x + advance > width && pen_x > 0.0 {
                let break_at = last_break.unwrap_or(glyphs.len());
                lines.push(LineBox {
                    top: line_top(line_index),
                    bottom: line_top(line_index) + line_height,
                    baseline: line_top(line_index) + line_height * 0.8,
                    start: line_start,
                });
                line_index += 1;
                line_start = glyphs.get(break_at).map_or(source, |g| g.source);
                // Re-flow the glyphs after the break onto the new line.
                let mut new_pen = 0.0f32;
                for g in glyphs.iter_mut().skip(break_at) {
                    g.line = line_index;
                    g.x = new_pen;
                    new_pen += g.advance;
                }
                pen_x = new_pen;
                last_break = None;
            }

            glyphs.push(PlacedGlyph {
                kind: face,
                px,
                index,
                ink,
                source,
                units: unit_count,
                x: pen_x,
                advance,
                line: line_index,
            });
            pen_x += advance;
            if ch == ' ' {
                last_break = Some(glyphs.len());
            }
        }

        // Final line.
        lines.push(LineBox {
            top: line_top(line_index),
            bottom: line_top(line_index) + line_height,
            baseline: line_top(line_index) + line_height * 0.8,
            start: line_start,
        });

        let height = lines.len() as f32 * line_height;
        Layout { glyphs, lines, text_len, height }
    }

    // --- Layout (geometry copied verbatim from the historical backends) ----

    /// Rebuilds the display list for the current viewport width.
    ///
    /// Args:
    ///     view: Client size in DIPs.
    pub(crate) fn relayout(&mut self, view: (f32, f32)) {
        self.view = view;
        self.items.clear();

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

                    let (base, size, line_height) = Self::style_for(*flavor);
                    let base_ink = match flavor {
                        Flavor::Heading(_) => Ink::Strong,
                        _ => Ink::Text,
                    };
                    let ink = if *quote_depth > 0 { Ink::Muted } else { base_ink };

                    let inner = if *flavor == Flavor::Code { avail - CODE_PAD * 2.0 } else { avail };
                    let layout =
                        self.build_layout(text, base, ink, size, line_height, inner.max(40.0));
                    let height = layout.height;

                    if *flavor == Flavor::Code {
                        let rect = RectF {
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
                            let rect = RectF {
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
                    let col_width = text_width / columns as f32;
                    let cell_width = (col_width - CELL_PAD_X * 2.0).max(30.0);
                    let table_top = y;

                    for (row_index, row) in rows.iter().enumerate() {
                        let mut laid = Vec::new();
                        let mut row_height: f32 = 0.0;
                        for cell in &row.cells {
                            let (base, ink) = if row.header {
                                (FaceKind::SansBold, Ink::Strong)
                            } else {
                                (FaceKind::Sans, Ink::Text)
                            };
                            let layout = self.build_layout(
                                cell,
                                base,
                                ink,
                                BODY_SIZE,
                                BODY_LINE,
                                cell_width,
                            );
                            row_height = row_height.max(layout.height);
                            laid.push(layout);
                        }
                        let row_bottom = y + row_height + CELL_PAD_Y * 2.0;

                        if row.header {
                            self.items.push(Item {
                                draw: Draw::Rect {
                                    rect: RectF {
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
                                    source: Source::Cell {
                                        block: index,
                                        row: row_index,
                                        cell: column,
                                    },
                                },
                                top: y,
                                bottom: row_bottom,
                            });
                        }

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

                    for column in 0..=columns {
                        let cx = origin_x + column as f32 * col_width;
                        self.items.push(Item {
                            draw: Draw::Line {
                                x0: cx,
                                y0: table_top,
                                x1: cx,
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
        self.rematch(false);
    }

    fn max_scroll(&self) -> f32 {
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

    fn in_gutter(&self, x: f32) -> bool {
        x >= self.view.0 - SCROLLBAR_W
    }

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

    fn press_gutter(&mut self, y: f32) -> bool {
        let Some(t) = self.thumb() else {
            return false;
        };
        if y >= t.top && y <= t.top + t.height {
            self.drag_grab = Some(y - t.top);
            self.hover_thumb = true;
            true
        } else {
            self.drag_grab = Some(t.height / 2.0);
            self.drag_to(y);
            self.hover_thumb = true;
            true
        }
    }

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

    fn scroll_by(&mut self, delta: f32) -> bool {
        let before = self.scroll;
        self.scroll = (self.scroll + delta).max(0.0).min(self.max_scroll());
        self.scroll != before
    }

    fn scroll_to(&mut self, offset: f32) -> bool {
        let before = self.scroll;
        self.scroll = offset.max(0.0).min(self.max_scroll());
        self.scroll != before
    }

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
    fn hit_text(&self, point: (f32, f32)) -> Option<(usize, u32)> {
        let doc_y = point.1 + self.scroll;
        for (index, item) in self.items.iter().enumerate() {
            if doc_y < item.top || doc_y > item.bottom {
                continue;
            }
            let Draw::Text { layout, x, y, .. } = &item.draw else {
                continue;
            };
            let (offset, inside) = layout.hit_test(point.0 - x, doc_y - y);
            if inside {
                return Some((index, offset));
            }
        }
        None
    }

    fn item_len(&self, index: usize) -> u32 {
        match self.items.get(index).map(|item| &item.draw) {
            Some(Draw::Text { source, .. }) => {
                self.text_of(*source).map_or(0, |text| text.utf16.len() as u32)
            }
            _ => 0,
        }
    }

    /// Caret position nearest a viewport point.
    fn caret_at(&self, point: (f32, f32)) -> Option<Caret> {
        let doc_y = point.1 + self.scroll;
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
        let (offset, _) = layout.hit_test(point.0 - x, doc_y - y);
        Some(Caret { item: index, pos: offset.min(self.item_len(index)) })
    }

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

    fn selected_text(&self) -> String {
        let Some(selection) = self.selection.filter(|s| !s.is_empty()) else {
            return String::new();
        };
        let (start, end) = selection.ordered();
        let mut out: Vec<u16> = Vec::new();
        let mut previous: Option<Source> = None;

        for index in start.item..=end.item {
            let Some(Draw::Text { source, .. }) = self.items.get(index).map(|item| &item.draw)
            else {
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
                    (
                        Source::Cell { block: b1, row: r1, .. },
                        Source::Cell { block: b2, row: r2, .. }
                    ) if b1 == b2 && r1 == r2
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

    fn jump_to_anchor(&mut self, fragment: &str) -> bool {
        let wanted = percent_decode(fragment).to_lowercase();
        let Some(&(_, block)) = self.anchors.iter().find(|(slug, _)| slug.to_lowercase() == wanted)
        else {
            return false;
        };
        let top = self.block_tops.get(block).copied().unwrap_or(0.0);
        self.scroll_to(top - BLOCK_GAP)
    }

    fn open_search(&mut self) {
        if self.search.is_none() {
            self.search = Some(Search { query: String::new(), matches: Vec::new(), current: 0 });
        }
    }

    fn edit_query(&mut self, edit: impl FnOnce(&mut String)) {
        if let Some(search) = self.search.as_mut() {
            edit(&mut search.query);
            self.rematch(true);
        }
    }

    /// Recomputes matches and their highlight rectangles.
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
                        let rects = layout.range_rects(at as u32, needle.len() as u32, (*x, *y));
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

    fn bar_rect(&self) -> RectF {
        let right = self.view.0 - SCROLLBAR_W - BAR_MARGIN;
        RectF {
            left: (right - BAR_W).max(BAR_MARGIN),
            top: BAR_MARGIN,
            right,
            bottom: BAR_MARGIN + BAR_H,
        }
    }

    fn bar_has_arrows(&self) -> bool {
        self.search.as_ref().is_some_and(|s| s.matches.len() > 1)
    }

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

    // --- Rasterisation -----------------------------------------------------

    /// Ensures the frame buffer matches the device size for `view` at `scale`.
    fn ensure_buffer(&mut self, scale: f32) {
        let w = (self.view.0 * scale).round().max(1.0) as u32;
        let h = (self.view.1 * scale).round().max(1.0) as u32;
        if w != self.width || h != self.height {
            self.width = w;
            self.height = h;
            self.pixels = vec![0u32; (w as usize) * (h as usize)];
        }
    }

    /// Alpha-composites `color` (`0xAARRGGBB`) over the pixel at `(px, py)`.
    ///
    /// Args:
    ///     px: Device x, must be in range.
    ///     py: Device y, must be in range.
    ///     color: Source colour with straight alpha in the top byte.
    ///     coverage: Extra 0..=255 coverage multiplier (glyph anti-aliasing).
    fn blend(&mut self, px: u32, py: u32, color: u32, coverage: u8) {
        if px >= self.width || py >= self.height {
            return;
        }
        let a = ((color >> 24) & 0xFF) * u32::from(coverage) / 255;
        if a == 0 {
            return;
        }
        let idx = (py as usize) * (self.width as usize) + (px as usize);
        let dst = self.pixels[idx];
        let sr = (color >> 16) & 0xFF;
        let sg = (color >> 8) & 0xFF;
        let sb = color & 0xFF;
        let dr = (dst >> 16) & 0xFF;
        let dg = (dst >> 8) & 0xFF;
        let db = dst & 0xFF;
        let blend = |s: u32, d: u32| ((s * a + d * (255 - a)) / 255) & 0xFF;
        let r = blend(sr, dr);
        let g = blend(sg, dg);
        let b = blend(sb, db);
        self.pixels[idx] = 0xFF00_0000 | (r << 16) | (g << 8) | b;
    }

    /// Fills a rectangle in viewport DIPs with a solid or translucent ink.
    ///
    /// Args:
    ///     rect: Rectangle in viewport DIPs.
    ///     color: Colour as `0xAARRGGBB`.
    fn fill_rect(&mut self, rect: RectF, color: u32) {
        let scale = self.scale;
        let x0 = (rect.left * scale).round().max(0.0) as i32;
        let y0 = (rect.top * scale).round().max(0.0) as i32;
        let x1 = (rect.right * scale).round().min(self.width as f32) as i32;
        let y1 = (rect.bottom * scale).round().min(self.height as f32) as i32;
        for py in y0.max(0)..y1 {
            for px in x0.max(0)..x1 {
                self.blend(px as u32, py as u32, color, 255);
            }
        }
    }

    /// Fills a rounded rectangle; corners use a per-pixel distance test.
    ///
    /// Args:
    ///     rect: Rectangle in viewport DIPs.
    ///     radius: Corner radius in DIPs.
    ///     color: Colour as `0xAARRGGBB`.
    fn fill_round_rect(&mut self, rect: RectF, radius: f32, color: u32) {
        if radius <= 0.0 {
            self.fill_rect(rect, color);
            return;
        }
        let scale = self.scale;
        let r = radius * scale;
        let x0 = (rect.left * scale).max(0.0);
        let y0 = (rect.top * scale).max(0.0);
        let x1 = (rect.right * scale).min(self.width as f32);
        let y1 = (rect.bottom * scale).min(self.height as f32);
        let px0 = x0.floor() as i32;
        let py0 = y0.floor() as i32;
        let px1 = x1.ceil() as i32;
        let py1 = y1.ceil() as i32;
        for py in py0.max(0)..py1.min(self.height as i32) {
            for px in px0.max(0)..px1.min(self.width as i32) {
                let cx = px as f32 + 0.5;
                let cy = py as f32 + 0.5;
                // Distance into the nearest corner circle, if in a corner.
                let dx = if cx < x0 + r {
                    x0 + r - cx
                } else if cx > x1 - r {
                    cx - (x1 - r)
                } else {
                    0.0
                };
                let dy = if cy < y0 + r {
                    y0 + r - cy
                } else if cy > y1 - r {
                    cy - (y1 - r)
                } else {
                    0.0
                };
                let dist = (dx * dx + dy * dy).sqrt();
                // Anti-alias the one-pixel boundary.
                let coverage = if dist <= r - 0.5 {
                    255
                } else if dist >= r + 0.5 {
                    0
                } else {
                    ((r + 0.5 - dist) * 255.0) as u8
                };
                if coverage > 0 {
                    self.blend(px as u32, py as u32, color, coverage);
                }
            }
        }
    }

    /// Draws a horizontal or vertical hairline as a filled band.
    fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: u32) {
        let half = (width / 2.0).max(0.5);
        let rect = if (y0 - y1).abs() < (x0 - x1).abs() {
            // Horizontal.
            RectF { left: x0.min(x1), top: y0 - half, right: x0.max(x1), bottom: y0 + half }
        } else {
            // Vertical.
            RectF { left: x0 - half, top: y0.min(y1), right: x0 + half, bottom: y0.max(y1) }
        };
        self.fill_rect(rect, color);
    }

    /// Draws one laid-out run at a viewport position.
    ///
    /// Args:
    ///     glyphs: The run's placed glyphs.
    ///     baselines: Per-line baselines, in DIPs.
    ///     x: Viewport x of the run's left edge, in DIPs.
    ///     y: Viewport y of the run's top edge, in DIPs.
    ///     default_ink: Ink for glyphs with no inline override.
    ///     clip: Optional device-pixel clip `(x0, y0, x1, y1)`.
    fn draw_layout(
        &mut self,
        glyphs: &[PlacedGlyph],
        baselines: &[f32],
        x: f32,
        y: f32,
        default_ink: Ink,
        clip: Option<(i32, i32, i32, i32)>,
    ) {
        let scale = self.scale;
        for glyph in glyphs {
            // Resolve the rasterised glyph (cached).
            let raster = match self.fonts.glyph(glyph.kind, glyph.px, glyph.index) {
                Some(g) => g,
                None => continue,
            };
            let gw = raster.width;
            let gh = raster.rows;
            let left = raster.left;
            let top = raster.top;
            // Copy coverage out so the borrow on `self.fonts` ends before blend.
            let coverage = raster.coverage.clone();
            if gw <= 0 || gh <= 0 {
                continue;
            }

            let baseline = baselines.get(glyph.line).copied().unwrap_or(0.0);
            let pen_x = (x + glyph.x) * scale;
            let pen_y = (y + baseline) * scale;
            let origin_x = pen_x.round() as i32 + left;
            let origin_y = pen_y.round() as i32 - top;
            let color = ink_color(if glyph.ink == Ink::Text { default_ink } else { glyph.ink });

            for row in 0..gh {
                let py = origin_y + row;
                if py < 0 || py >= self.height as i32 {
                    continue;
                }
                for col in 0..gw {
                    let px = origin_x + col;
                    if px < 0 || px >= self.width as i32 {
                        continue;
                    }
                    if let Some((cx0, cy0, cx1, cy1)) = clip {
                        if px < cx0 || px >= cx1 || py < cy0 || py >= cy1 {
                            continue;
                        }
                    }
                    let cov = coverage[(row * gw + col) as usize];
                    if cov > 0 {
                        self.blend(px as u32, py as u32, color, cov);
                    }
                }
            }
        }
    }

    /// Draws the whole frame, then is blitted by the window.
    ///
    /// Args:
    ///     scale: Device pixels per DIP for this frame.
    pub(crate) fn paint(&mut self, scale: f32) {
        self.scale = scale;
        self.ensure_buffer(scale);
        // Clear to the page background.
        let bg = opaque(BG);
        for px in self.pixels.iter_mut() {
            *px = bg;
        }

        let top = self.scroll;
        let bottom = self.scroll + self.view.1;

        // Collect the draw work first so the immutable borrow on `self.items`
        // is released before the mutable rasterisation calls.
        enum Job {
            MatchRect(RectF, Ink),
            SelectionRect(RectF),
            Glyphs { glyphs: Vec<PlacedGlyph>, baselines: Vec<f32>, x: f32, y: f32, ink: Ink },
            Rect(RectF, Ink, f32),
            Line(f32, f32, f32, f32, Ink, f32),
        }
        let mut jobs: Vec<Job> = Vec::new();

        let search_matches: Vec<(usize, Vec<RectF>, bool)> = match self.search.as_ref() {
            Some(search) => search
                .matches
                .iter()
                .enumerate()
                .map(|(k, m)| (m.item, m.rects.clone(), k == search.current))
                .collect(),
            None => Vec::new(),
        };

        for (index, item) in self.items.iter().enumerate() {
            if item.bottom < top || item.top > bottom {
                continue;
            }
            // Match highlights under this text item.
            if matches!(item.draw, Draw::Text { .. }) {
                for (m_item, rects, is_current) in &search_matches {
                    if *m_item != index {
                        continue;
                    }
                    let ink = if *is_current { Ink::MatchCurrent } else { Ink::Match };
                    for rect in rects {
                        jobs.push(Job::MatchRect(
                            RectF {
                                left: rect.left,
                                top: rect.top - self.scroll,
                                right: rect.right,
                                bottom: rect.bottom - self.scroll,
                            },
                            ink,
                        ));
                    }
                }
            }
            // Selection highlight.
            if let Draw::Text { layout, x, y, .. } = &item.draw {
                if let Some((start, len)) = self.selected_range(index) {
                    for rect in layout.range_rects(start, len, (*x, *y - self.scroll)) {
                        jobs.push(Job::SelectionRect(rect));
                    }
                }
            }
            match &item.draw {
                Draw::Text { layout, x, y, ink, .. } => {
                    let baselines: Vec<f32> = layout.lines.iter().map(|l| l.baseline).collect();
                    jobs.push(Job::Glyphs {
                        glyphs: layout.glyphs.clone(),
                        baselines,
                        x: *x,
                        y: *y - self.scroll,
                        ink: *ink,
                    });
                }
                Draw::Rect { rect, ink, radius } => {
                    jobs.push(Job::Rect(
                        RectF {
                            left: rect.left,
                            top: rect.top - self.scroll,
                            right: rect.right,
                            bottom: rect.bottom - self.scroll,
                        },
                        *ink,
                        *radius,
                    ));
                }
                Draw::Line { x0, y0, x1, y1, ink, width } => {
                    jobs.push(Job::Line(
                        *x0,
                        *y0 - self.scroll,
                        *x1,
                        *y1 - self.scroll,
                        *ink,
                        *width,
                    ));
                }
            }
        }

        for job in jobs {
            match job {
                Job::MatchRect(rect, ink) => self.fill_rect(rect, ink_color(ink)),
                Job::SelectionRect(rect) => self.fill_rect(rect, ink_color(Ink::Selection)),
                Job::Glyphs { glyphs, baselines, x, y, ink } => {
                    self.draw_layout(&glyphs, &baselines, x, y, ink, None)
                }
                Job::Rect(rect, ink, radius) => {
                    self.fill_round_rect(rect, radius, ink_color(ink))
                }
                Job::Line(x0, y0, x1, y1, ink, width) => {
                    self.draw_line(x0, y0, x1, y1, width, ink_color(ink))
                }
            }
        }

        // Scrollbar on top.
        if let Some(t) = self.thumb() {
            let left = self.view.0 - SCROLLBAR_W;
            self.fill_rect(
                RectF { left, top: 0.0, right: self.view.0, bottom: self.view.1 },
                ink_color(Ink::Background),
            );
            let inset = 1.0;
            let radius = (SCROLLBAR_W - inset * 2.0) / 2.0;
            let ink = if self.hover_thumb || self.drag_grab.is_some() {
                Ink::ThumbHover
            } else {
                Ink::Thumb
            };
            self.fill_round_rect(
                RectF {
                    left: left + inset,
                    top: t.top,
                    right: self.view.0 - inset,
                    bottom: t.top + t.height,
                },
                radius,
                ink_color(ink),
            );
        }

        // Search bar.
        if self.search.is_some() {
            self.paint_bar();
        }
    }

    /// Draws the Ctrl+F bar: query with caret, match count, and arrows.
    fn paint_bar(&mut self) {
        let bar = self.bar_rect();
        let mid = (bar.top + bar.bottom) / 2.0;
        let has_arrows = self.bar_has_arrows();
        let (query, count_text) = {
            let search = self.search.as_ref().unwrap();
            let count = if search.query.is_empty() {
                None
            } else if search.matches.is_empty() {
                Some(String::from("0/0"))
            } else {
                Some(format!("{}/{}", search.current + 1, search.matches.len()))
            };
            (search.query.clone(), count)
        };

        // Surface + border.
        self.fill_round_rect(bar, 6.0, ink_color(Ink::Surface));
        self.stroke_round_rect(bar, 6.0, ink_color(Ink::Border));

        let mut right = bar.right - BAR_PAD;
        if has_arrows {
            for (glyph, slot) in [('\u{25B2}', 2.0_f32), ('\u{25BC}', 1.0)] {
                let gx = bar.right - BAR_ARROW_W * slot + (BAR_ARROW_W - 10.0) / 2.0;
                self.draw_simple_text(&glyph.to_string(), gx, mid - 6.0, 10.0, Ink::Muted, None);
            }
            right = bar.right - BAR_ARROW_W * 2.0 - 4.0;
        }

        if let Some(count) = count_text {
            let width = self.measure_text(&count, 13.0);
            right -= width;
            self.draw_simple_text(&count, right, mid - 7.0, 13.0, Ink::Muted, None);
            right -= 8.0;
        }

        // Query, clipped to its field and scrolled so the caret stays visible.
        let field_left = bar.left + BAR_PAD;
        let field_width = (right - field_left).max(10.0);
        let text_width = self.measure_text(&query, 14.0);
        let shift = (text_width - field_width + 2.0).max(0.0);
        let scale = self.scale;
        let clip = (
            (field_left * scale) as i32,
            (bar.top * scale) as i32,
            ((field_left + field_width) * scale) as i32,
            (bar.bottom * scale) as i32,
        );
        self.draw_simple_text(&query, field_left - shift, mid - 7.0, 14.0, Ink::Text, Some(clip));
        // Caret just past the query.
        let caret_x = field_left - shift + text_width + 1.0;
        self.draw_line(caret_x, bar.top + 7.0, caret_x, bar.bottom - 7.0, 1.0, ink_color(Ink::Text));
    }

    /// Strokes a rounded rectangle outline one DIP wide.
    fn stroke_round_rect(&mut self, rect: RectF, radius: f32, color: u32) {
        // Draw the filled rounded rect, then punch the interior back to the
        // surface colour, leaving a 1px ring.
        let outer = rect;
        let inner = RectF {
            left: rect.left + 1.0,
            top: rect.top + 1.0,
            right: rect.right - 1.0,
            bottom: rect.bottom - 1.0,
        };
        self.fill_round_rect(outer, radius, color);
        self.fill_round_rect(inner, (radius - 1.0).max(0.0), ink_color(Ink::Surface));
    }

    /// Measures the advance width of a short single-line string in DIPs.
    ///
    /// Args:
    ///     text: Text to measure (no wrapping).
    ///     size: Font size in DIPs.
    ///
    /// Returns:
    ///     Total advance in DIPs.
    fn measure_text(&mut self, text: &str, size: f32) -> f32 {
        let scale = self.scale;
        let px = (size * scale).round().max(1.0) as u32;
        let mut width = 0.0f32;
        for ch in text.chars() {
            let index = self.fonts.glyph_index(FaceKind::Sans, u32::from(ch));
            width += self.fonts.glyph(FaceKind::Sans, px, index).map(|g| g.advance).unwrap_or(0.0);
        }
        width / scale
    }

    /// Draws a short single-line string in the sans face.
    ///
    /// Args:
    ///     text: Text to draw.
    ///     x: Viewport x of the left edge, in DIPs.
    ///     y: Viewport y of the top edge, in DIPs.
    ///     size: Font size in DIPs.
    ///     ink: Ink to draw with.
    ///     clip: Optional device-pixel clip `(x0, y0, x1, y1)`.
    fn draw_simple_text(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        size: f32,
        ink: Ink,
        clip: Option<(i32, i32, i32, i32)>,
    ) {
        let scale = self.scale;
        let px = (size * scale).round().max(1.0) as u32;
        let baseline = size; // one-line box: baseline near the glyph cap top + size
        let mut glyphs = Vec::new();
        let mut pen = 0.0f32;
        for ch in text.chars() {
            let index = self.fonts.glyph_index(FaceKind::Sans, u32::from(ch));
            let advance = self
                .fonts
                .glyph(FaceKind::Sans, px, index)
                .map(|g| g.advance / scale)
                .unwrap_or(0.0);
            glyphs.push(PlacedGlyph {
                kind: FaceKind::Sans,
                px,
                index,
                ink,
                source: 0,
                units: 1,
                x: pen,
                advance,
                line: 0,
            });
            pen += advance;
        }
        self.draw_layout(&glyphs, &[baseline], x, y, ink, clip);
    }

    /// Resizes the viewport (device buffer is reallocated on next paint).
    ///
    /// Args:
    ///     scale: Device pixels per DIP.
    pub(crate) fn resize(&mut self, scale: f32) {
        self.scale = scale;
    }
}

// ---------------------------------------------------------------------------
// Link actions and the shared event loop
// ---------------------------------------------------------------------------

/// Opens an http(s) URL in the user's default browser.
///
/// The self-exec and document launch below are identical on every platform,
/// so they live here; only the browser hand-off differs, so it is split by
/// `cfg`: `xdg-open` on Unix, `ShellExecuteW` on Windows.
///
/// Args:
///     url: An absolute http or https URL; see `classify_link`.
#[cfg(unix)]
fn open_in_browser(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Opens an http(s) URL in the user's default browser via `ShellExecuteW`.
///
/// Args:
///     url: An absolute http or https URL; see `classify_link`.
#[cfg(windows)]
fn open_in_browser(url: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    let url = HSTRING::from(url);
    // SAFETY: a documented shell call; all pointers are valid for the call and
    // the result (an HINSTANCE-sized status) is intentionally ignored.
    unsafe {
        ShellExecuteW(None, w!("open"), &url, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Opens a markdown file in a new QuickGlass process, detached.
///
/// Launches this same executable so that relative `.md` links open in a fresh
/// window. Identical on both platforms (a GUI-subsystem exe on Windows shows
/// no console).
///
/// Args:
///     path: The file to open.
fn open_document(path: &Path) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Ends autoscroll, if active, and restores the normal cursor.
///
/// Args:
///     autoscroll: Current autoscroll state; cleared on return.
///     host: Window whose timer is stopped and cursor restored.
///
/// Returns:
///     `true` if autoscroll was active, so the caller can swallow the input
///     that ended it.
fn stop_autoscroll<H: Host>(autoscroll: &mut Option<Autoscroll>, host: &H) -> bool {
    if autoscroll.take().is_none() {
        return false;
    }
    host.stop_timer();
    host.set_cursor(Cursor::Arrow);
    true
}

// Win32-style virtual-key codes the viewer responds to; both shells map their
// native keys onto these so the handling below is shared.
const VK_BACK: u16 = 0x08;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_SPACE: u16 = 0x20;
const VK_PRIOR: u16 = 0x21;
const VK_NEXT: u16 = 0x22;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_UP: u16 = 0x26;
const VK_DOWN: u16 = 0x28;
const VK_A: u16 = 0x41;
const VK_C: u16 = 0x43;
const VK_F: u16 = 0x46;
const VK_F3: u16 = 0x72;

/// Drives the event loop, wiring input to the renderer, and exits the process
/// on close.
///
/// This is the one behaviour loop for both platforms: the shell translates its
/// native events into [`Ev`], and this closure applies them to the renderer and
/// repaints as needed.
///
/// Args:
///     host: The created, shown window implementing [`Host`].
///     renderer: The renderer with the document laid out and first frame drawn.
///
/// Returns:
///     Never; the process exits on [`Ev::Close`].
pub(crate) fn drive<H: Host + 'static>(host: H, renderer: Renderer) -> ! {
    let mut renderer = renderer;
    let mut autoscroll: Option<Autoscroll> = None;
    let mut pressed_link: Option<String> = None;
    let mut hover_cursor = Cursor::Arrow;
    let mut press_point = (0.0_f32, 0.0_f32);
    let mut swallow_char = false;

    host.run_loop(move |host, event| {
        let scale = host.scale();
        match event {
            Ev::Timer => {
                if let Some(active) = autoscroll.as_mut() {
                    let now = Instant::now();
                    let elapsed = now.duration_since(active.last_step).as_secs_f32();
                    active.last_step = now;
                    if renderer.scroll_by(active.velocity(renderer.cursor.1) * elapsed) {
                        paint_present(host, &mut renderer, scale);
                    }
                }
            }

            Ev::Close => std::process::exit(0),

            Ev::Resized(width, height) => {
                renderer.resize(scale);
                renderer.relayout((width as f32 / scale, height as f32 / scale));
                paint_present(host, &mut renderer, scale);
            }

            Ev::Paint => paint_present(host, &mut renderer, scale),

            Ev::Moved(x, y) => {
                renderer.cursor = (x, y);
                let moved = renderer.drag_to(y);
                let hover_changed = renderer.update_hover();
                if moved || hover_changed {
                    paint_present(host, &mut renderer, scale);
                }
                if renderer.selecting {
                    let (dx, dy) = (x - press_point.0, y - press_point.1);
                    if (dx * dx + dy * dy).sqrt() > DRAG_THRESHOLD {
                        // Moving off the press point turns a link click into a
                        // selection.
                        pressed_link = None;
                    }
                    if pressed_link.is_none() {
                        // Dragging past the top or bottom edge scrolls.
                        let edge = if y < 0.0 {
                            y
                        } else if y > renderer.view.1 {
                            y - renderer.view.1
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
                        paint_present(host, &mut renderer, scale);
                    }
                }
                if autoscroll.is_none() && renderer.drag_grab.is_none() {
                    let cursor = renderer.cursor;
                    let icon = if renderer.selecting && pressed_link.is_none() {
                        Cursor::IBeam
                    } else if renderer.link_at(cursor).is_some() {
                        Cursor::Hand
                    } else if !renderer.in_gutter(cursor.0)
                        && renderer.bar_hit(cursor).is_none()
                        && renderer.hit_text(cursor).is_some()
                    {
                        Cursor::IBeam
                    } else {
                        Cursor::Arrow
                    };
                    if icon != hover_cursor {
                        hover_cursor = icon;
                        host.set_cursor(icon);
                    }
                }
            }

            Ev::Left => {
                if autoscroll.is_some() || renderer.selecting {
                    return;
                }
                renderer.cursor = (-1.0, -1.0);
                if renderer.update_hover() {
                    paint_present(host, &mut renderer, scale);
                }
            }

            Ev::Button { button, down, at } => {
                renderer.cursor = at;
                match (button, down) {
                    (Button::Left, true) => {
                        if stop_autoscroll(&mut autoscroll, host) {
                            // The click only cancels autoscroll.
                        } else if let Some(hit) = renderer.bar_hit(renderer.cursor) {
                            if hit != BarHit::Inside {
                                renderer.step_search(hit == BarHit::Next);
                                paint_present(host, &mut renderer, scale);
                            }
                        } else if renderer.in_gutter(renderer.cursor.0) {
                            let gy = renderer.cursor.1;
                            if renderer.press_gutter(gy) {
                                paint_present(host, &mut renderer, scale);
                            }
                        } else {
                            pressed_link = renderer.link_at(renderer.cursor).map(str::to_owned);
                            press_point = renderer.cursor;
                            let had_selection = renderer.selection.is_some_and(|s| !s.is_empty());
                            renderer.selection = renderer
                                .caret_at(renderer.cursor)
                                .map(|caret| Selection { anchor: caret, focus: caret });
                            renderer.selecting = true;
                            if had_selection {
                                paint_present(host, &mut renderer, scale);
                            }
                        }
                    }
                    (Button::Left, false) => {
                        renderer.selecting = false;
                        if renderer.drag_grab.take().is_some() {
                            renderer.update_hover();
                            paint_present(host, &mut renderer, scale);
                        }
                        let Some(dest) = pressed_link.take() else {
                            return;
                        };
                        if renderer.link_at(renderer.cursor) != Some(dest.as_str()) {
                            return;
                        }
                        match classify_link(&dest, renderer.base_dir.as_deref()) {
                            LinkAction::Anchor(fragment) => {
                                if renderer.jump_to_anchor(&fragment) {
                                    paint_present(host, &mut renderer, scale);
                                }
                            }
                            LinkAction::Browser(url) => open_in_browser(&url),
                            LinkAction::Document(file) => open_document(&file),
                            LinkAction::Ignore => {}
                        }
                    }
                    (Button::Middle, true) => {
                        if !stop_autoscroll(&mut autoscroll, host) {
                            let active = Autoscroll::new(renderer.cursor.1);
                            host.set_cursor(Cursor::SizeNs);
                            hover_cursor = Cursor::Arrow;
                            host.start_timer(AUTOSCROLL_FRAME.as_millis() as u32);
                            autoscroll = Some(active);
                        }
                    }
                    (Button::Middle, false) => {
                        let held = autoscroll
                            .as_ref()
                            .is_some_and(|active| active.pressed_at.elapsed() >= AUTOSCROLL_HOLD);
                        if held {
                            stop_autoscroll(&mut autoscroll, host);
                        }
                    }
                    (Button::Other, true) => {
                        stop_autoscroll(&mut autoscroll, host);
                    }
                    (Button::Other, false) => {}
                }
            }

            Ev::Wheel(notches) => {
                stop_autoscroll(&mut autoscroll, host);
                if renderer.scroll_by(-notches * BODY_LINE * 3.0) {
                    paint_present(host, &mut renderer, scale);
                }
            }

            Ev::Key { vk, ctrl, shift } => {
                if stop_autoscroll(&mut autoscroll, host) {
                    swallow_char = true;
                    return;
                }
                swallow_char = false;

                if ctrl && vk == VK_F {
                    renderer.open_search();
                    paint_present(host, &mut renderer, scale);
                    return;
                }
                if ctrl && vk == VK_C {
                    let text = renderer.selected_text();
                    if !text.is_empty() {
                        host.set_selection(&text);
                    }
                    return;
                }
                if ctrl && vk == VK_A {
                    renderer.select_all();
                    paint_present(host, &mut renderer, scale);
                    return;
                }
                if renderer.search.is_some() {
                    match vk {
                        VK_ESCAPE => {
                            renderer.search = None;
                            paint_present(host, &mut renderer, scale);
                            return;
                        }
                        VK_RETURN | VK_F3 => {
                            renderer.step_search(!shift);
                            paint_present(host, &mut renderer, scale);
                            return;
                        }
                        VK_BACK => {
                            renderer.edit_query(|query| {
                                query.pop();
                            });
                            paint_present(host, &mut renderer, scale);
                            return;
                        }
                        // Space is typed into the query (it arrives as Ev::Char)
                        // rather than paging down.
                        VK_SPACE => return,
                        _ => {}
                    }
                }

                let page = renderer.view.1 * 0.9;
                let dy = match vk {
                    VK_DOWN => BODY_LINE * 3.0,
                    VK_UP => -BODY_LINE * 3.0,
                    VK_NEXT | VK_SPACE => page,
                    VK_PRIOR => -page,
                    VK_HOME => -renderer.content_height,
                    VK_END => renderer.content_height,
                    VK_ESCAPE => std::process::exit(0),
                    _ => 0.0,
                };
                if dy != 0.0 && renderer.scroll_by(dy) {
                    paint_present(host, &mut renderer, scale);
                }
            }

            Ev::Char(c) => {
                if std::mem::take(&mut swallow_char) {
                    return;
                }
                if renderer.search.is_some() && !c.is_control() {
                    renderer.edit_query(|query| query.push(c));
                    paint_present(host, &mut renderer, scale);
                }
            }
        }
    })
}

/// Paints the current frame and blits it to the window.
///
/// Args:
///     host: Target window.
///     renderer: Renderer to paint with.
///     scale: Device pixels per DIP.
fn paint_present<H: Host>(host: &H, renderer: &mut Renderer, scale: f32) {
    renderer.paint(scale);
    let (pixels, w, h) = renderer.frame();
    host.present(pixels, w, h);
}
