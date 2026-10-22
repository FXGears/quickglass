# QuickGlass

[![Release](https://img.shields.io/github/v/release/FXGears/quickglass?style=flat-square)](https://github.com/FXGears/quickglass/releases)
[![License](https://img.shields.io/github/license/FXGears/quickglass?style=flat-square)](LICENSE)
[![Binary Size](https://img.shields.io/badge/binary-~600_KB-blue?style=flat-square)]()
[![No Electron](https://img.shields.io/badge/electron-none-green?style=flat-square)]()

**Under 1 MB. ~30ms startup. Windows and Linux. Opens a markdown file. Closes when you're done.**

Every other markdown tool wants to be your editor, your notebook, your second brain, or your publishing pipeline. QuickGlass just opens the file.

No editor. No tabs. No sidebar. No plugins. No settings. No config files. No internet. No telemetry. No decisions. Dark theme only.

![QuickGlass screenshot](resources/screenshot.png)

## Install

Download from [Releases](https://github.com/FXGears/quickglass/releases).

- Windows: `quickglass.exe`. No installer, it's one file.
- Debian/Ubuntu: `sudo apt install ./quickglass_<version>_amd64.deb`
- Fedora/RHEL/openSUSE: `sudo dnf install ./quickglass-<version>-1.x86_64.rpm` (or `zypper install`)

The Linux packages install `/usr/bin/quickglass` and register it for `.md` and text files, so it shows up under "Open with" in KDE, GNOME and other freedesktop file managers. Needs X11 (libX11, libXext, FreeType, fontconfig, pulled in as dependencies) and glibc 2.34+. `quickglass-linux-x86_64` is the bare binary for other distros (`chmod +x` after download).

Set as default on Windows:

1. Right-click any `.md` file → Open with → Choose another app
2. Browse to `quickglass.exe`
3. Check "Always use this app"

Done.

## How It Compares

| | **QuickGlass** | *Typora* | *Marktext* | *Tinta* | *Obsidian* |
|---|---|---|---|---|---|
| Opens a markdown file | **✓** | *✓* | *✓* | *✓* | *✓* |
| Binary size | **~600 KB** | *90 MB* | *180 MB* | *1.8 MB* | *300+ MB* |
| Startup time | **~30ms** | *~2s* | *~3s* | *~100ms* | *~4s* |
| Electron inside | **No** | *No* | *Yes* | *No* | *Yes* |
| Wants to manage your files | **No** | *No* | *No* | *No* | *Yes* |
| Has a settings page | **No** | *Yes* | *Yes* | *Yes* | *Yes* |
| Has plugin system | **No** | *No* | *Yes* | *No* | *Yes* |
| Requires account | **No** | *No* | *No* | *No* | *Optional (but it'll ask)* |
| Phones home | **No** | *No* | *No* | *No* | *Yes* |
| **Features you'll never use** | **0** | **47** | **63** | **28** | **∞** |

## Non-Goals

These will never be added:

- Editing
- Tabs
- File watching
- Themes
- Mermaid / LaTeX / diagrams
- Export
- Plugin system

If you want those things, use [Tinta](https://tinta.cc), [Markpad](https://github.com/alecdotdev/Markpad), or [Typora](https://typora.io). They're good at being everything. QuickGlass is good at being nothing except fast.

## Specs

| | |
|---|---|
| Binary | ~600 KB |
| Memory | ~19 MB resident, ~5 MB private (Linux), one process |
| Cold start | ~30ms |
| Platforms | Windows 10+; Linux on X11 or Wayland via XWayland (glibc 2.34+) |
| Runtime deps | Windows: none. Linux: libX11, libXext, FreeType, fontconfig |
| Language | Rust |
| Renderer | One shared CPU rasteriser on both platforms; FreeType glyphs |
| License | GPL-3.0 |

## Building

```
cargo build --release
```

Requires Rust (stable). Windows also needs Visual Studio Build Tools; Linux needs the X11, FreeType and fontconfig dev packages (`libx11-dev libxext-dev libfreetype-dev libfontconfig-dev` on Debian/Ubuntu).

Release binaries use a size-optimized standard library (nightly toolchain with `rust-src`):

- Windows host: `powershell -ExecutionPolicy Bypass -File build-release.ps1`
- Linux host: `./build-release.sh` builds both the Linux binary and the Windows `.exe` (via mingw-w64), then `packaging/linux/build-packages.sh` builds the `.deb` and `.rpm`.
