# Changelog

## v1.0.0

**New engine.** WebView2 is gone. QuickGlass now draws the page itself with Direct2D and
DirectWrite, which are part of Windows, in a plain Win32 window. Same viewer, same
dark theme, a fraction of the weight.

| | 0.11.0 | 1.0.0 |
|---|---|---|
| Binary | 858,624 bytes | 227,328 bytes |
| Window painted | ~478 ms | ~55 ms |
| Processes | 7 | 1 |
| Private memory | 211.5 MB | 15.6 MB |
| Profile on disk | `%TEMP%\quickglass-<pid>` | none |

New:

- Ctrl+F search bar with match count and next/previous.
- Drag to select text, including across blocks and table cells. Ctrl+C copies, Ctrl+A
  selects all.
- Links: `#anchors` jump to headings, web links open your browser, relative `.md` links
  open in a new QuickGlass window.
- Middle-click autoscroll, either click-to-toggle or hold-and-drag.

What changed for you:

- Nothing to install or configure. WebView2 is no longer used.
- No profile folder is created under `%TEMP%` any more. Any `quickglass-<pid>` folders
  left there by 0.11.0 are safe to delete.
- The `QUICKGLASS_TRACE` startup log is now a build-time option (`--features trace`)
  and is absent from release binaries.

## v0.11.0

**Renamed to QuickGlass.** The project was previously called ViewMD, a name shared with
several other markdown viewers (including one on the Mac App Store). QuickGlass is the
same tool under a distinct name.

What changed for you:

- The binary is now `quickglass.exe`.
- The window title and usage text read QuickGlass.
- The per-process WebView2 profile folder is now `%TEMP%\quickglass-<pid>` instead of
  `viewmd-<pid>`.
- The startup trace variable is `QUICKGLASS_TRACE` and its log is
  `%TEMP%\quickglass-startup.log`.

If you had ViewMD set as your `.md` handler, re-point the association at
`quickglass.exe`. No behaviour changed; this is a rename plus the 0.10.1 icon.

## v0.10.1

New application and file-association icon. The tilted double-pane now glows neon
cyan on a transparent background instead of sitting washed-out on a dark box, so it
reads on both light and dark taskbars and Explorer backgrounds. Icons are generated
at every size the `.ico` carries, with the glow and stroke tuned per size so small
icons stay legible instead of collapsing into a blob. The social preview card now
carries the icon on its right side.

No code changes; binary behaviour is identical to 0.10.0.

## v0.10.0

**Startup no longer flashes white.** The window stays hidden until the document has
finished rendering, so it appears with content already painted instead of showing an
empty white page first. WebView2 paints a white surface underneath web content before
any HTML loads; deferring the window past that is the only reliable fix.

**The WebView2 profile no longer accumulates.** It moved from a permanent folder in
`%LOCALAPPDATA%\ViewMD` to a per-process one under `%TEMP%`, reclaimed on a later
launch. The old folder grew to roughly 30 MB with use — about forty times the size of
the binary — and that growth was itself slowing down launches. Nothing persists between
runs now.

If you ran 0.9.x, `%LOCALAPPDATA%\ViewMD` is left behind and is safe to delete by hand.
Nothing writes to it any more and new installs never create it.

**Closing the window reliably exits.** Previously the message pump could block with the
process still alive, holding its profile folder. A real mouse click always worked, so
this was only visible to scripts.

**Optional native renderer, off by default** (`--beta_render`, requires a build with
`--features beta_render`). A Direct2D/DirectWrite renderer that skips WebView2 entirely.
It is not in stock releases and adds nothing to them. See
[BETA-RENDERER.md](BETA-RENDERER.md).

**Startup timing instrumentation** behind the `VIEWMD_TRACE` environment variable.
Inert unless set, costing one environment lookup.

Fixed a version mismatch: `Cargo.toml` had been left at `0.1.0` since before the first
release.

## v0.9.1

- App icon: double-pane glass viewport, steel blue on dark navy
- Icon embedded in binary (title bar, taskbar, file explorer)
- Improved list item spacing for wrapped text
- Dark scrollbar styling
- Replaced `image` crate with `png` for smaller binary (814KB → 758KB)
- No console window on launch

## v0.9.0

Initial release.

Written in a single sitting to solve a problem: we had markdown files and no way to read them without launching an IDE or spinning up a local server. Every existing option was either an Electron app pretending to be lightweight, a note-taking tool that wanted to manage our files, or a browser extension asking for filesystem access.

We wanted the markdown equivalent of double-clicking a PDF. Open. Read. Close.

Rust, WebView2, pulldown-cmark, ~120 lines of code. Dark mode. GitHub-width content. No menu. No tabs. No settings.

---

## Design Principles

1. **If it doesn't serve reading, it doesn't ship.**
2. **The binary stays under 1MB.**
3. **Startup stays under 100ms.**
4. **Zero configuration.**
5. **No network access.**
