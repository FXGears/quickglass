# The Beta Renderer

An optional Direct2D/DirectWrite renderer that replaces WebView2. It is **not in stock
releases**, is **off by default**, and adds **nothing** to a normal build.

If you downloaded `quickglass.exe` from Releases, this page does not apply to you. Nothing
here changes what you have.

## Why

WebView2 is a browser. QuickGlass uses it to draw a page of text that never changes, never
runs script, and never goes to the network. That is a lot of machinery for the job.

Measured on one machine, time from process start to a window with painted content:

| Renderer | Time | Of which is renderer init |
|---|---|---|
| WebView2 | ~488 ms | 358 ms building the webview, 113 ms loading the page |
| Native | ~233 ms | 181 ms initialising Direct2D |

Parsing the markdown takes 0.5 ms and laying it out takes 7 ms. Under 3% of startup is
QuickGlass's own work; the rest is renderer initialisation. That is the entire argument for
the native path, and it is why the WebView2 number did not improve by tuning our code.

Both figures come from the in-process trace, not from watching for a window handle to
appear. See [Measuring](#measuring).

## Building and running

The renderer is gated at compile time. A stock build does not contain it — `mod native`
is not compiled and the Direct2D/DirectWrite bindings are not linked.

```powershell
cargo build --release                          # stock, WebView2 only
cargo build --release --features beta_render    # native available
```

Then select it per launch:

```powershell
.\target\release\quickglass.exe file.md                  # WebView2 (default, both builds)
.\target\release\quickglass.exe file.md --beta_render    # native (feature build only)
```

WebView2 stays the default even in a feature build. A stock build accepts
`--beta_render` and ignores it rather than failing — the binary has no console to report
an error to.

Carrying the renderer costs **20 KB** (775.5 KB → 795.5 KB). Direct2D and DirectWrite
are OS components, so there is no runtime payload and no DLL to ship.

## How it works

Markdown becomes a flat list of blocks. Each block becomes an `IDWriteTextLayout`.
Everything flattens into a display list of text, rectangle, and line primitives in
document coordinates, and painting draws only the slice that intersects the viewport.

Styling is hardcoded from the same constants the CSS uses. There is no cascade, no
selector matching, and no general box model. That omission is what keeps it to 20 KB.

## What it renders

Headings 1–6 with rules under h1 and h2, paragraphs, bold, italic, inline code, links,
fenced and indented code blocks in rounded panels, nested blockquotes with bars, ordered
and unordered lists with hanging indents, horizontal rules, and tables with a filled
header row and grid lines.

Scrolling works by wheel, arrow keys, PageUp/PageDown, Home/End, and Space. Escape
closes. The scrollbar is 10 px with a proportional thumb that highlights on hover,
supports dragging, and jumps when the bare track is clicked.

## What it does not render

Deliberately absent:

- Text selection and clipboard
- Clickable links (they are styled, not active)
- Images — `[image: alt]` appears instead
- Auto-sized table columns
- Syntax highlighting

Known gaps, found by reading the parser against what `Options::all()` emits, all
currently unhandled:

- **Strikethrough** — `Tag::Strikethrough` is never handled
- **Math** — `InlineMath` and `DisplayMath` are dropped
- **Task list markers** — dropped
- **Footnotes** — references dropped; bodies leak in as paragraphs
- **Definition lists** — flatten to paragraphs

Code blocks wrap here where the WebView2 path scrolls horizontally.

Differences observed against WebView2 that are **not** yet explained, and may be bugs:

- Content sits about 24 px further right
- Content starts about 24 px higher, and the offset accumulates down the page
- Inline code has no rounded background chip

## Comparing the two renderers

`resources/markdown-test.md` is a 23-section fixture covering CommonMark plus every
extension `Options::all()` enables. Each section states what it tests and what correct
output looks like. Open it in both renderers and compare.

**WebView2 is the reference.** It is a real browser and it is what ships, so its output
defines correct.

Pixel-exact comparison is not achievable. Chromium's rasteriser and DirectWrite differ
in glyph positioning, hinting, and subpixel antialiasing, so any check demanding zero
difference will fail on correct output. Compare side by side and judge by eye.

If you automate screenshots, note that `PrintWindow` does not work for the native
window. It drives `WM_PRINT`/`WM_PRINTCLIENT`, and a Direct2D HWND render target never
responds, so the client area comes back blank. WebView2 captures correctly with
`PW_RENDERFULLCONTENT` (`0x2`), which is mandatory — without it both come back blank.
Capturing the foreground window from the screen works for both.

## Measuring

```powershell
$env:QUICKGLASS_TRACE = '1'
.\target\release\quickglass.exe .\resources\markdown-test.md --beta_render
Get-Content "$env:TEMP\quickglass-startup.log"
```

One line per launch, with cumulative milliseconds per phase. Both renderers share the
same clock, so their traces compare directly. The variable is inert when unset.

Do not time startup by polling for a window handle from outside the process. The handle
appears well before content is painted and produced figures roughly fifteen times too
optimistic.

## Status

Beta. It renders the common cases correctly and is roughly twice as fast to first paint,
but it has the gaps listed above and its visual output has not been checked section by
section against WebView2.

It has not been decided whether this becomes the default, stays behind a flag, or is
dropped. That call comes after the visual comparison is done.
