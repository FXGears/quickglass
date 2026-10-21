//! Minimal X11 window, event loop, clipboard, and software blit (Linux).
//!
//! This is the Linux shell for the shared renderer in [`crate::render`]. It
//! owns a single top-level X11 window, translates raw Xlib events into the
//! small [`Ev`] enum the renderer consumes, and presents CPU-rendered frames to
//! the screen. Frames are uploaded with MIT-SHM (`XShmPutImage`) when the server
//! offers it, and fall back to a plain `XPutImage` otherwise, so the code never
//! hard-depends on the shared-memory extension.
//!
//! Only the stock X11 client ABI is used — `libX11` (via the `x11` crate) and
//! `libXext` (MIT-SHM, bound by direct FFI in [`shm`]) — with no XWayland- or
//! compositor-specific calls, so it runs under XLibre as well.
//!
//! The window implements [`crate::render::Host`] (size, scale, present, cursor,
//! timer, selection, event loop) and [`crate::render::FontSource`] (the four
//! face files, resolved through Fontconfig). Everything visual — layout, paint,
//! hit-testing, search, selection — lives in [`crate::render`].

use std::cell::Cell;
use std::ffi::{CString, c_char, c_int, c_uchar, c_uint, c_ulong, c_void};
use std::path::Path;
use std::time::{Duration, Instant};

use x11::keysym;
use x11::xlib;

use crate::render::{Button, Cursor, Ev, FaceKind, FontSource, Host, Renderer};

// X cursor-font glyph indices (from `X11/cursorfont.h`), not exposed by the
// `x11` crate, so declared here.
const XC_LEFT_PTR: c_uint = 68;
const XC_HAND2: c_uint = 60;
const XC_XTERM: c_uint = 152;
const XC_SB_V_DOUBLE_ARROW: c_uint = 116;

// ---------------------------------------------------------------------------
// MIT-SHM FFI (libXext). The `x11` crate exposes these through `dlopen`-only
// helpers, so the few entry points and the segment struct are bound directly;
// `build.rs` links `libXext`. All are stock X11 client calls.
// ---------------------------------------------------------------------------

mod shm {
    use std::ffi::{c_char, c_int, c_uint, c_ulong};

    use x11::xlib::{Bool, Display, Drawable, GC, Visual, XImage};

    /// The shared-memory segment descriptor exchanged with the X server; the
    /// field layout matches `XShmSegmentInfo` from `X11/extensions/XShm.h`.
    #[repr(C)]
    pub struct XShmSegmentInfo {
        pub shmseg: c_ulong,
        pub shmid: c_int,
        pub shmaddr: *mut c_char,
        pub read_only: Bool,
    }

    #[link(name = "Xext")]
    unsafe extern "C" {
        pub fn XShmQueryExtension(display: *mut Display) -> Bool;
        pub fn XShmAttach(display: *mut Display, info: *mut XShmSegmentInfo) -> Bool;
        pub fn XShmDetach(display: *mut Display, info: *mut XShmSegmentInfo) -> Bool;
        pub fn XShmCreateImage(
            display: *mut Display,
            visual: *mut Visual,
            depth: c_uint,
            format: c_int,
            data: *mut c_char,
            info: *mut XShmSegmentInfo,
            width: c_uint,
            height: c_uint,
        ) -> *mut XImage;
        pub fn XShmPutImage(
            display: *mut Display,
            drawable: Drawable,
            gc: GC,
            image: *mut XImage,
            src_x: c_int,
            src_y: c_int,
            dst_x: c_int,
            dst_y: c_int,
            width: c_uint,
            height: c_uint,
            send_event: Bool,
        ) -> Bool;
    }
}

// System V shared memory, via libc (already a transitive dependency); declared
// directly to avoid widening the crate's own dependency set.
unsafe extern "C" {
    fn shmget(key: c_int, size: usize, shmflg: c_int) -> c_int;
    fn shmat(shmid: c_int, shmaddr: *const c_void, shmflg: c_int) -> *mut c_void;
    fn shmdt(shmaddr: *const c_void) -> c_int;
    fn shmctl(shmid: c_int, cmd: c_int, buf: *mut c_void) -> c_int;
}

const IPC_PRIVATE: c_int = 0;
const IPC_CREAT: c_int = 0o1000;
const IPC_RMID: c_int = 0;

/// A live MIT-SHM frame buffer shared with the X server.
///
/// Holds the attached segment, its client-side mapping, and the `XImage` that
/// wraps it. Dropped in reverse order of setup so the server detaches before
/// the segment is removed.
struct ShmSurface {
    display: *mut xlib::Display,
    info: Box<shm::XShmSegmentInfo>,
    image: *mut xlib::XImage,
    /// Mapped segment, reinterpreted as 32-bit pixels for direct writes.
    pixels: *mut u32,
    width: u32,
    height: u32,
}

impl Drop for ShmSurface {
    /// Detaches from the server, destroys the image, and releases the mapping.
    ///
    /// Runs when a surface is replaced by a larger one on resize; the leaked
    /// window never drops its final surface, so process exit leaves that one
    /// segment for the kernel to reclaim (it was already marked `IPC_RMID`).
    fn drop(&mut self) {
        // SAFETY: all handles were created together in `try_shm` and are freed
        // exactly once here, in reverse order of setup.
        unsafe {
            shm::XShmDetach(self.display, self.info.as_mut() as *mut shm::XShmSegmentInfo);
            if !self.image.is_null() {
                xlib::XDestroyImage(self.image);
            }
            if !self.info.shmaddr.is_null() {
                shmdt(self.info.shmaddr as *const c_void);
            }
        }
    }
}

/// The application window and everything the renderer needs to drive it.
pub(crate) struct Window {
    display: *mut xlib::Display,
    window: xlib::Window,
    gc: xlib::GC,
    screen: c_int,
    visual: *mut xlib::Visual,
    depth: c_int,
    wm_delete: xlib::Atom,
    /// Clipboard target atoms, interned once.
    atoms: Atoms,
    cursors: Cursors,
    /// Current client size in physical pixels, tracked from ConfigureNotify.
    size: Cell<(u32, u32)>,
    /// MIT-SHM surface, when the extension is available. `RefCell` so the
    /// shared `&Window` the event loop holds can still reattach it on resize.
    shm: std::cell::RefCell<Option<ShmSurface>>,
    /// Scratch `XImage` for the non-SHM fallback path.
    fallback: Cell<*mut xlib::XImage>,
    /// Text currently offered on the CLIPBOARD selection, answered on request.
    clipboard: std::cell::RefCell<String>,
    /// Software timer: interval and the deadline of the next [`Ev::Timer`].
    timer: Cell<Option<(Duration, Instant)>>,
}

/// Interned atoms used for window management and clipboard transfer.
struct Atoms {
    clipboard: xlib::Atom,
    targets: xlib::Atom,
    utf8_string: xlib::Atom,
    text: xlib::Atom,
    string: xlib::Atom,
    wm_protocols: xlib::Atom,
}

/// Pre-created cursor handles, one per [`Cursor`] shape.
struct Cursors {
    arrow: xlib::Cursor,
    hand: xlib::Cursor,
    ibeam: xlib::Cursor,
    size_ns: xlib::Cursor,
}

/// Interns an atom by name.
///
/// Args:
///     display: Open X display.
///     name: Atom name.
///
/// Returns:
///     The atom id, or `0` if interning fails.
fn intern(display: *mut xlib::Display, name: &str) -> xlib::Atom {
    let cname = CString::new(name).unwrap_or_default();
    // SAFETY: `display` is a live display; `cname` outlives the call.
    unsafe { xlib::XInternAtom(display, cname.as_ptr(), xlib::False) }
}

/// The window icon as a packed little-endian `[w, h, w*h ARGB]` u32 stream,
/// pre-decoded from `resources/icon-64.png`. Embedding the raw pixels avoids
/// linking a PNG decoder, mirroring how the Windows build embeds a prebuilt
/// `.ico` rather than decoding at runtime.
const ICON_ARGB: &[u8] = include_bytes!("../resources/icon-64.argb");

/// Sets `_NET_WM_ICON` so the window manager can show the app icon in the
/// title bar, taskbar, and Alt-Tab.
///
/// The property is a `CARDINAL` array whose first two elements are the width
/// and height and whose remaining `w*h` elements are `0xAARRGGBB` pixels. On
/// an LP64 system `XChangeProperty` with format 32 reads each element as a
/// `long`, so the embedded 32-bit words are widened to `c_ulong` here.
///
/// A malformed or truncated blob is skipped rather than drawn partially.
///
/// Args:
///     display: Live X display.
///     window: The window to set the property on.
fn set_net_wm_icon(display: *mut xlib::Display, window: xlib::Window) {
    let word = |i: usize| -> u32 {
        let b = &ICON_ARGB[i * 4..i * 4 + 4];
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    };
    if ICON_ARGB.len() < 8 || ICON_ARGB.len() % 4 != 0 {
        return;
    }
    let (w, h) = (word(0) as usize, word(1) as usize);
    let total = 2 + w.saturating_mul(h);
    if ICON_ARGB.len() / 4 != total {
        return;
    }
    // [w, h, pixels...], each widened to the long X expects for format 32.
    let data: Vec<c_ulong> = (0..total).map(|i| word(i) as c_ulong).collect();
    let net_wm_icon = intern(display, "_NET_WM_ICON");
    // SAFETY: `data` outlives the call; `net_wm_icon`/`XA_CARDINAL` are valid
    // atoms and `data.len()` matches the element count passed to X.
    unsafe {
        xlib::XChangeProperty(
            display,
            window,
            net_wm_icon,
            xlib::XA_CARDINAL,
            32,
            xlib::PropModeReplace,
            data.as_ptr() as *const c_uchar,
            data.len() as c_int,
        );
    }
}

impl Window {
    /// Opens a hidden top-level window of `size` DIPs with the page background.
    ///
    /// Args:
    ///     title: Window title, set as both `WM_NAME` and `_NET_WM_NAME`.
    ///     size: Initial client width and height in DIPs.
    ///
    /// Returns:
    ///     The window, or a message describing which X call failed.
    pub(crate) fn create(title: &str, size: (f32, f32)) -> Result<Self, String> {
        // SAFETY: a single, self-contained block of Xlib setup. Every pointer
        // used below is validated (display non-null) before further use, and
        // all handles created here are owned by the returned `Window`.
        unsafe {
            xlib::XrmInitialize();
            let display = xlib::XOpenDisplay(std::ptr::null());
            if display.is_null() {
                return Err("XOpenDisplay failed: no X11 display (is $DISPLAY set?)".into());
            }

            let screen = xlib::XDefaultScreen(display);
            let root = xlib::XRootWindow(display, screen);
            let visual = xlib::XDefaultVisual(display, screen);
            let depth = xlib::XDefaultDepth(display, screen);

            let scale = scale_for(display, screen);
            let width = (size.0 * scale).round().max(1.0) as c_uint;
            let height = (size.1 * scale).round().max(1.0) as c_uint;

            let background = c_ulong::from(crate::doc::BG);
            let window =
                xlib::XCreateSimpleWindow(display, root, 0, 0, width, height, 0, 0, background);
            if window == 0 {
                xlib::XCloseDisplay(display);
                return Err("XCreateSimpleWindow failed".into());
            }

            // Title: legacy WM_NAME plus the modern UTF-8 _NET_WM_NAME.
            let ctitle = CString::new(title).unwrap_or_default();
            xlib::XStoreName(display, window, ctitle.as_ptr());
            let net_wm_name = intern(display, "_NET_WM_NAME");
            let utf8_string = intern(display, "UTF8_STRING");
            xlib::XChangeProperty(
                display,
                window,
                net_wm_name,
                utf8_string,
                8,
                xlib::PropModeReplace,
                title.as_ptr(),
                title.len() as c_int,
            );

            // Icon: _NET_WM_ICON is a CARDINAL array [w, h, w*h ARGB pixels],
            // read by the WM for the title bar, taskbar and Alt-Tab. Uses the
            // same 64px glass-pane icon the Windows build embeds via its .ico,
            // pre-decoded into resources/icon-64.argb (a packed little-endian
            // u32 stream whose first two words are the dimensions), so no PNG
            // decoder is linked at runtime.
            set_net_wm_icon(display, window);

            // Ask the WM to route close requests to us rather than killing the
            // client, so the event loop can exit cleanly.
            let wm_protocols = intern(display, "WM_PROTOCOLS");
            let mut wm_delete = intern(display, "WM_DELETE_WINDOW");
            xlib::XSetWMProtocols(display, window, &mut wm_delete, 1);

            xlib::XSelectInput(
                display,
                window,
                xlib::ExposureMask
                    | xlib::KeyPressMask
                    | xlib::ButtonPressMask
                    | xlib::ButtonReleaseMask
                    | xlib::PointerMotionMask
                    | xlib::StructureNotifyMask
                    | xlib::LeaveWindowMask,
            );

            let gc = xlib::XCreateGC(display, window, 0, std::ptr::null_mut());
            if gc.is_null() {
                xlib::XDestroyWindow(display, window);
                xlib::XCloseDisplay(display);
                return Err("XCreateGC failed".into());
            }

            let cursors = Cursors {
                arrow: xlib::XCreateFontCursor(display, XC_LEFT_PTR),
                hand: xlib::XCreateFontCursor(display, XC_HAND2),
                ibeam: xlib::XCreateFontCursor(display, XC_XTERM),
                size_ns: xlib::XCreateFontCursor(display, XC_SB_V_DOUBLE_ARROW),
            };

            let atoms = Atoms {
                clipboard: intern(display, "CLIPBOARD"),
                targets: intern(display, "TARGETS"),
                utf8_string,
                text: intern(display, "TEXT"),
                string: intern(display, "STRING"),
                wm_protocols,
            };

            let window = Window {
                display,
                window,
                gc,
                screen,
                visual,
                depth,
                wm_delete,
                atoms,
                cursors,
                size: Cell::new((width, height)),
                shm: std::cell::RefCell::new(None),
                fallback: Cell::new(std::ptr::null_mut()),
                clipboard: std::cell::RefCell::new(String::new()),
                timer: Cell::new(None),
            };

            // Best-effort SHM setup; a failure leaves `shm` as `None` and the
            // fallback path is used instead.
            *window.shm.borrow_mut() = window.try_shm(width, height);
            Ok(window)
        }
    }

    /// Physical pixels per DIP for this display, clamped to a sane range.
    pub(crate) fn scale(&self) -> f32 {
        // SAFETY: `display`/`screen` are valid for the window's lifetime.
        unsafe { scale_for(self.display, self.screen) }
    }

    /// Client area size in physical pixels.
    pub(crate) fn client_size(&self) -> (u32, u32) {
        self.size.get()
    }

    /// Maps the window and flushes so it becomes visible.
    pub(crate) fn show(&self) {
        // SAFETY: valid display and window.
        unsafe {
            xlib::XMapWindow(self.display, self.window);
            xlib::XFlush(self.display);
        }
    }

    /// Sets the cursor shown over the client area.
    pub(crate) fn set_cursor(&self, cursor: Cursor) {
        let handle = match cursor {
            Cursor::Arrow => self.cursors.arrow,
            Cursor::Hand => self.cursors.hand,
            Cursor::IBeam => self.cursors.ibeam,
            Cursor::SizeNs => self.cursors.size_ns,
        };
        // SAFETY: valid display, window, and cursor handle.
        unsafe {
            xlib::XDefineCursor(self.display, self.window, handle);
            xlib::XFlush(self.display);
        }
    }

    /// Starts the software timer that synthesises [`Ev::Timer`].
    ///
    /// Args:
    ///     ms: Interval between ticks, in milliseconds.
    pub(crate) fn start_timer(&self, ms: u32) {
        let interval = Duration::from_millis(u64::from(ms));
        self.timer.set(Some((interval, Instant::now() + interval)));
    }

    /// Stops the software timer.
    pub(crate) fn stop_timer(&self) {
        self.timer.set(None);
    }

    /// Takes ownership of the CLIPBOARD selection and stores `text` to answer
    /// future `SelectionRequest` events.
    ///
    /// Args:
    ///     text: UTF-8 text to offer to other clients.
    pub(crate) fn set_selection(&self, text: &str) {
        *self.clipboard.borrow_mut() = text.to_owned();
        // SAFETY: valid display, window, and interned atom.
        unsafe {
            xlib::XSetSelectionOwner(
                self.display,
                self.atoms.clipboard,
                self.window,
                xlib::CurrentTime,
            );
            xlib::XFlush(self.display);
        }
    }

    /// Uploads a frame to the window.
    ///
    /// The renderer produces premultiplied `0xAARRGGBB` words; a little-endian
    /// 32-bit TrueColor X visual reads the same memory as BGRA byte order, so
    /// the channels line up and whole `u32`s are copied as-is.
    ///
    /// Takes `&self` so the leaked, shared window the event loop holds can
    /// present every frame; the SHM surface lives behind a `RefCell`.
    ///
    /// Args:
    ///     pixels: Frame as packed `0xAARRGGBB` words, `w * h` long.
    ///     w: Frame width in pixels.
    ///     h: Frame height in pixels.
    pub(crate) fn present(&self, pixels: &[u32], w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        // Reattach SHM if the window grew past the current segment.
        let needs_bigger =
            self.shm.borrow().as_ref().is_some_and(|s| s.width < w || s.height < h);
        if needs_bigger {
            *self.shm.borrow_mut() = self.try_shm(w, h);
        }

        if let Some(surface) = self.shm.borrow().as_ref() {
            if surface.width >= w && surface.height >= h && !surface.pixels.is_null() {
                let count = (w as usize) * (h as usize);
                // SAFETY: the segment holds `surface.width * surface.height`
                // pixels and `w*h <= that`; both regions are valid for `count`.
                unsafe {
                    std::ptr::copy_nonoverlapping(pixels.as_ptr(), surface.pixels, count);
                    (*surface.image).width = w as c_int;
                    (*surface.image).height = h as c_int;
                    (*surface.image).bytes_per_line = (w * 4) as c_int;
                    shm::XShmPutImage(
                        self.display,
                        self.window,
                        self.gc,
                        surface.image,
                        0,
                        0,
                        0,
                        0,
                        w,
                        h,
                        xlib::False,
                    );
                    xlib::XFlush(self.display);
                }
                return;
            }
        }
        self.present_fallback(pixels, w, h);
    }

    /// Uploads a frame with a plain `XImage` when SHM is unavailable.
    fn present_fallback(&self, pixels: &[u32], w: u32, h: u32) {
        let old = self.fallback.replace(std::ptr::null_mut());
        // SAFETY: `old`, when non-null, is an XImage this window created and
        // owns; the fresh buffer is handed to XCreateImage to own and free.
        unsafe {
            if !old.is_null() {
                xlib::XDestroyImage(old);
            }
            let bytes = (w as usize) * (h as usize) * 4;
            let buffer = malloc(bytes);
            if buffer.is_null() {
                return;
            }
            std::ptr::copy_nonoverlapping(pixels.as_ptr() as *const u8, buffer as *mut u8, bytes);
            let image = xlib::XCreateImage(
                self.display,
                self.visual,
                self.depth as c_uint,
                xlib::ZPixmap,
                0,
                buffer as *mut c_char,
                w,
                h,
                32,
                (w * 4) as c_int,
            );
            if image.is_null() {
                free(buffer);
                return;
            }
            xlib::XPutImage(self.display, self.window, self.gc, image, 0, 0, 0, 0, w, h);
            xlib::XFlush(self.display);
            self.fallback.set(image);
        }
    }

    /// Attempts to create and attach an MIT-SHM surface of at least `w`×`h`.
    ///
    /// Args:
    ///     w: Required width in pixels.
    ///     h: Required height in pixels.
    ///
    /// Returns:
    ///     A live surface, or `None` if the extension is missing or any step
    ///     fails (the caller then uses the `XPutImage` fallback).
    fn try_shm(&self, w: u32, h: u32) -> Option<ShmSurface> {
        let width = w.max(1);
        let height = h.max(1);
        let bytes = (width as usize) * (height as usize) * 4;
        // SAFETY: MIT-SHM and System V shm calls, each checked before use; the
        // segment is detached and removed on every partial-failure path.
        unsafe {
            if shm::XShmQueryExtension(self.display) == xlib::False {
                return None;
            }
            let mut info = Box::new(shm::XShmSegmentInfo {
                shmseg: 0,
                shmid: 0,
                shmaddr: std::ptr::null_mut(),
                read_only: xlib::False,
            });
            let shmid = shmget(IPC_PRIVATE, bytes, IPC_CREAT | 0o600);
            if shmid < 0 {
                return None;
            }
            info.shmid = shmid;
            let addr = shmat(shmid, std::ptr::null(), 0);
            if addr as isize == -1 {
                shmctl(shmid, IPC_RMID, std::ptr::null_mut());
                return None;
            }
            info.shmaddr = addr as *mut c_char;

            let image = shm::XShmCreateImage(
                self.display,
                self.visual,
                self.depth as c_uint,
                xlib::ZPixmap,
                info.shmaddr,
                info.as_mut() as *mut shm::XShmSegmentInfo,
                width,
                height,
            );
            if image.is_null() {
                shmdt(addr);
                shmctl(shmid, IPC_RMID, std::ptr::null_mut());
                return None;
            }

            if shm::XShmAttach(self.display, info.as_mut() as *mut shm::XShmSegmentInfo)
                == xlib::False
            {
                xlib::XDestroyImage(image);
                shmdt(addr);
                shmctl(shmid, IPC_RMID, std::ptr::null_mut());
                return None;
            }
            xlib::XSync(self.display, xlib::False);
            // Mark for deletion now; the kernel reclaims it once both the server
            // and this process detach, which survives an unclean exit.
            shmctl(shmid, IPC_RMID, std::ptr::null_mut());

            Some(ShmSurface {
                display: self.display,
                info,
                image,
                pixels: addr as *mut u32,
                width,
                height,
            })
        }
    }

    /// Answers a clipboard `SelectionRequest` with the stored text.
    ///
    /// Supports the `TARGETS`, `UTF8_STRING`, `STRING`, and `TEXT` targets;
    /// any other target is refused with a zero `property`.
    fn answer_selection(&self, request: &xlib::XSelectionRequestEvent) {
        let text = self.clipboard.borrow();
        let mut property = request.property;
        // XA_ATOM is the fixed atom id 4 (`X11/Xatom.h`).
        const XA_ATOM: xlib::Atom = 4;
        // SAFETY: fields come from a real event; atoms are interned; the reply
        // is a stack `XEvent` passed by pointer to XSendEvent.
        unsafe {
            if request.target == self.atoms.targets {
                let targets: [xlib::Atom; 4] = [
                    self.atoms.targets,
                    self.atoms.utf8_string,
                    self.atoms.string,
                    self.atoms.text,
                ];
                xlib::XChangeProperty(
                    self.display,
                    request.requestor,
                    request.property,
                    XA_ATOM,
                    32,
                    xlib::PropModeReplace,
                    targets.as_ptr() as *const c_uchar,
                    targets.len() as c_int,
                );
            } else if request.target == self.atoms.utf8_string
                || request.target == self.atoms.string
                || request.target == self.atoms.text
            {
                xlib::XChangeProperty(
                    self.display,
                    request.requestor,
                    request.property,
                    request.target,
                    8,
                    xlib::PropModeReplace,
                    text.as_ptr(),
                    text.len() as c_int,
                );
            } else {
                property = 0;
            }

            let mut reply: xlib::XEvent = std::mem::zeroed();
            reply.selection = xlib::XSelectionEvent {
                type_: xlib::SelectionNotify,
                serial: 0,
                send_event: xlib::True,
                display: self.display,
                requestor: request.requestor,
                selection: request.selection,
                target: request.target,
                property,
                time: request.time,
            };
            xlib::XSendEvent(self.display, request.requestor, xlib::False, 0, &mut reply);
            xlib::XFlush(self.display);
        }
    }

    /// Translates one Xlib event into zero or more [`Ev`]s and dispatches them.
    fn handle_event(&self, event: &xlib::XEvent, handler: &mut impl FnMut(Ev)) {
        // SAFETY: the union variant read matches the event's `type_` tag.
        let kind = unsafe { event.type_ };
        match kind {
            xlib::ClientMessage => {
                let msg = unsafe { event.client_message };
                // data.l[0] carries the protocol atom; WM_DELETE_WINDOW closes.
                if msg.message_type == self.atoms.wm_protocols
                    && msg.data.get_long(0) as xlib::Atom == self.wm_delete
                {
                    handler(Ev::Close);
                }
            }
            xlib::ConfigureNotify => {
                let cfg = unsafe { event.configure };
                let (w, h) = (cfg.width.max(0) as u32, cfg.height.max(0) as u32);
                if (w, h) != self.size.get() {
                    self.size.set((w, h));
                    handler(Ev::Resized(w, h));
                }
            }
            xlib::Expose => {
                // Coalesce: only repaint once the final expose in a burst lands.
                let expose = unsafe { event.expose };
                if expose.count == 0 {
                    handler(Ev::Paint);
                }
            }
            xlib::MotionNotify => {
                let motion = unsafe { event.motion };
                let scale = self.scale();
                handler(Ev::Moved(motion.x as f32 / scale, motion.y as f32 / scale));
            }
            xlib::LeaveNotify => handler(Ev::Left),
            xlib::ButtonPress => {
                let button = unsafe { event.button };
                let scale = self.scale();
                let at = (button.x as f32 / scale, button.y as f32 / scale);
                match button.button {
                    // Wheel up/down (and horizontal 6/7) arrive as buttons.
                    4 | 6 => handler(Ev::Wheel(1.0)),
                    5 | 7 => handler(Ev::Wheel(-1.0)),
                    other => handler(Ev::Button { button: map_button(other), down: true, at }),
                }
            }
            xlib::ButtonRelease => {
                let button = unsafe { event.button };
                // Wheel "release" events carry no meaning; ignore them.
                if matches!(button.button, 4 | 5 | 6 | 7) {
                    return;
                }
                let scale = self.scale();
                let at = (button.x as f32 / scale, button.y as f32 / scale);
                handler(Ev::Button { button: map_button(button.button), down: false, at });
            }
            xlib::KeyPress => {
                let mut key = unsafe { event.key };
                let ctrl = key.state & xlib::ControlMask != 0;
                let shift = key.state & xlib::ShiftMask != 0;

                // Keysym from group 0 drives the virtual-key mapping; a separate
                // XLookupString call yields any typed character.
                let keysym = unsafe { xlib::XLookupKeysym(&mut key, 0) };
                if let Some(vk) = map_keysym(keysym as c_uint) {
                    handler(Ev::Key { vk, ctrl, shift });
                }

                let mut buf = [0u8; 16];
                let mut sym: xlib::KeySym = 0;
                let len = unsafe {
                    xlib::XLookupString(
                        &mut key,
                        buf.as_mut_ptr() as *mut c_char,
                        buf.len() as c_int,
                        &mut sym,
                        std::ptr::null_mut(),
                    )
                };
                if len > 0 {
                    if let Ok(text) = std::str::from_utf8(&buf[..len as usize]) {
                        for c in text.chars() {
                            if !c.is_control() {
                                handler(Ev::Char(c));
                            }
                        }
                    }
                }
            }
            xlib::SelectionRequest => {
                let request = unsafe { event.selection_request };
                self.answer_selection(&request);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Host + FontSource — the two contracts the shared renderer drives the shell
// through. The window methods are inherent (above); these impls expose them to
// `crate::render` and add the Linux-specific loop and font resolution.
// ---------------------------------------------------------------------------

impl Host for Window {
    fn scale(&self) -> f32 {
        Window::scale(self)
    }

    fn client_size(&self) -> (u32, u32) {
        Window::client_size(self)
    }

    fn show(&self) {
        Window::show(self)
    }

    fn set_cursor(&self, cursor: Cursor) {
        Window::set_cursor(self, cursor)
    }

    fn start_timer(&self, ms: u32) {
        Window::start_timer(self, ms)
    }

    fn stop_timer(&self) {
        Window::stop_timer(self)
    }

    fn set_selection(&self, text: &str) {
        Window::set_selection(self, text)
    }

    fn present(&self, pixels: &[u32], w: u32, h: u32) {
        Window::present(self, pixels, w, h)
    }

    /// Runs the X11 event loop, delivering every event to `handler`.
    ///
    /// Between X events the software timer is polled and [`Ev::Timer`]
    /// synthesised when due, so autoscroll ticks without a real X timer. The
    /// window is leaked so the renderer's FreeType/glyph-cache state is never
    /// torn down — the process exits on [`Ev::Close`] instead.
    ///
    /// Args:
    ///     handler: Called once per event on this thread.
    ///
    /// Returns:
    ///     Never; the process exits on [`Ev::Close`].
    fn run_loop(self, mut handler: impl FnMut(&Self, Ev) + 'static) -> ! {
        let window: &'static Window = Box::leak(Box::new(self));
        let display = window.display;
        let mut event: xlib::XEvent = unsafe { std::mem::zeroed() };

        loop {
            // Drain all pending X events first, then service the timer, keeping
            // input latency low while still ticking autoscroll.
            while unsafe { xlib::XPending(display) } > 0 {
                unsafe { xlib::XNextEvent(display, &mut event) };
                window.handle_event(&event, &mut |ev| handler(window, ev));
            }

            if let Some((interval, deadline)) = window.timer.get() {
                let now = Instant::now();
                if now >= deadline {
                    window.timer.set(Some((interval, now + interval)));
                    handler(window, Ev::Timer);
                    continue;
                }
                // A timer is active: nap briefly so it fires on time without a
                // busy spin, unless an event is already waiting.
                if unsafe { xlib::XPending(display) } == 0 {
                    std::thread::sleep(Duration::from_millis(4));
                }
            } else {
                // No timer: block until the next event arrives.
                unsafe { xlib::XNextEvent(display, &mut event) };
                window.handle_event(&event, &mut |ev| handler(window, ev));
            }
        }
    }
}

/// Fontconfig-backed font resolution for the Linux shell.
///
/// Zero-sized: Fontconfig keeps its own global configuration, so the resolver
/// holds no state. Each [`FaceKind`] maps to a generic family plus a weight and
/// slant request.
struct FcFontSource;

impl FontSource for FcFontSource {
    fn resolve(&self, kind: FaceKind) -> Option<(String, u32)> {
        let (family, bold, italic) = match kind {
            FaceKind::Sans => ("sans-serif", false, false),
            FaceKind::SansBold => ("sans-serif", true, false),
            FaceKind::SansItalic => ("sans-serif", false, true),
            FaceKind::Mono => ("monospace", false, false),
        };
        resolve_font(family, bold, italic)
    }
}

/// Resolves a font file for a family and style through Fontconfig.
///
/// Args:
///     family: Generic family name, e.g. `"sans-serif"` or `"monospace"`.
///     bold: Request a bold weight.
///     italic: Request an italic slant.
///
/// Returns:
///     The font file path and its face index, or `None` if no match is found.
fn resolve_font(family: &str, bold: bool, italic: bool) -> Option<(String, u32)> {
    use fontconfig_sys as fc;

    let cfamily = CString::new(family).ok()?;
    // SAFETY: a standard Fontconfig match sequence. Every returned pointer is
    // checked before use, the pattern is destroyed on every exit path, and the
    // strings fontconfig returns are copied out before that destruction.
    unsafe {
        let pattern = fc::FcNameParse(cfamily.as_ptr() as *const fc::FcChar8);
        if pattern.is_null() {
            return None;
        }
        let weight =
            if bold { fc::constants::FC_WEIGHT_BOLD } else { fc::constants::FC_WEIGHT_REGULAR };
        let slant =
            if italic { fc::constants::FC_SLANT_ITALIC } else { fc::constants::FC_SLANT_ROMAN };
        fc::FcPatternAddInteger(pattern, fc::constants::FC_WEIGHT.as_ptr(), weight);
        fc::FcPatternAddInteger(pattern, fc::constants::FC_SLANT.as_ptr(), slant);

        fc::FcConfigSubstitute(std::ptr::null_mut(), pattern, fc::FcMatchPattern);
        fc::FcDefaultSubstitute(pattern);

        let mut result: fc::FcResult = fc::FcResultMatch;
        let matched = fc::FcFontMatch(std::ptr::null_mut(), pattern, &mut result);
        fc::FcPatternDestroy(pattern);
        if matched.is_null() {
            return None;
        }

        let mut file: *mut fc::FcChar8 = std::ptr::null_mut();
        let mut index: c_int = 0;
        let got_file =
            fc::FcPatternGetString(matched, fc::constants::FC_FILE.as_ptr(), 0, &mut file)
                == fc::FcResultMatch
                && !file.is_null();
        if !got_file {
            fc::FcPatternDestroy(matched);
            return None;
        }
        let path = std::ffi::CStr::from_ptr(file as *const c_char).to_string_lossy().into_owned();
        if fc::FcPatternGetInteger(matched, fc::constants::FC_INDEX.as_ptr(), 0, &mut index)
            != fc::FcResultMatch
        {
            index = 0;
        }
        fc::FcPatternDestroy(matched);
        Some((path, index.max(0) as u32))
    }
}

/// Maps an X button number to the app's [`Button`].
fn map_button(button: c_uint) -> Button {
    match button {
        1 => Button::Left,
        2 => Button::Middle,
        _ => Button::Other,
    }
}

/// Maps an X keysym to the Win32-style virtual-key code `native.rs` expects.
///
/// Args:
///     keysym: An X11 keysym from group 0.
///
/// Returns:
///     The matching virtual-key code, or `None` for keys the app ignores
///     (typed text reaches it through [`Ev::Char`] instead).
fn map_keysym(keysym: c_uint) -> Option<u16> {
    Some(match keysym {
        keysym::XK_BackSpace => 0x08,
        keysym::XK_Return | keysym::XK_KP_Enter => 0x0D,
        keysym::XK_Escape => 0x1B,
        keysym::XK_space => 0x20,
        // Numpad nav keys (NumLock off) send the KP_* keysyms.
        keysym::XK_Prior | keysym::XK_KP_Prior => 0x21,
        keysym::XK_Next | keysym::XK_KP_Next => 0x22,
        keysym::XK_End | keysym::XK_KP_End => 0x23,
        keysym::XK_Home | keysym::XK_KP_Home => 0x24,
        keysym::XK_Up | keysym::XK_KP_Up => 0x26,
        keysym::XK_Down | keysym::XK_KP_Down => 0x28,
        keysym::XK_F3 => 0x72,
        // Letters: map the lowercase and uppercase keysyms used by shortcuts.
        keysym::XK_a | keysym::XK_A => 0x41,
        keysym::XK_c | keysym::XK_C => 0x43,
        keysym::XK_f | keysym::XK_F => 0x46,
        _ => return None,
    })
}

/// Computes physical pixels per DIP from the display's reported DPI.
///
/// Args:
///     display: Open X display.
///     screen: Default screen index.
///
/// Returns:
///     A scale factor clamped to `[1.0, 4.0]`, defaulting to `1.0` when the
///     server reports no usable physical size.
unsafe fn scale_for(display: *mut xlib::Display, screen: c_int) -> f32 {
    // SAFETY: caller guarantees a live display and valid screen index.
    unsafe {
        let px = xlib::XDisplayWidth(display, screen) as f32;
        let mm = xlib::XDisplayWidthMM(display, screen) as f32;
        if px <= 0.0 || mm <= 0.0 {
            return 1.0;
        }
        let dpi = px / mm * 25.4;
        let scale = dpi / 96.0;
        if scale.is_finite() { scale.clamp(1.0, 4.0) } else { 1.0 }
    }
}

// C allocator access for the XPutImage fallback buffer, which XDestroyImage
// frees with the C library's `free`.
unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Opens a window and renders `source` with the shared CPU renderer until
/// closed.
///
/// It creates the window, builds the renderer with Fontconfig-resolved faces,
/// parses the markdown, lays it out, paints once before showing, then hands the
/// window and renderer to the shared [`crate::render::drive`] loop. The
/// [`crate::StartupTrace`] marks match the Windows shell so both report on one
/// clock.
///
/// Args:
///     source: Markdown text to display.
///     title: Window title.
///     path: File the markdown came from, used to resolve relative links.
///     trace: Startup trace shared with `main`.
///
/// Returns:
///     Never; the process exits when the window is closed.
pub fn run(
    source: &str,
    title: &str,
    path: Option<&Path>,
    trace: std::rc::Rc<std::cell::RefCell<crate::StartupTrace>>,
) -> ! {
    let base_dir = path.and_then(Path::parent).map(Path::to_path_buf);
    let window = Window::create(title, (920.0, 700.0)).expect("Failed to create window");
    trace.borrow_mut().mark("window_created");

    let mut renderer = Renderer::new(&FcFontSource).expect("Failed to initialise renderer");
    trace.borrow_mut().mark("renderer_ready");

    let (blocks, anchors) = crate::doc::parse(source);
    renderer.set_document(blocks, anchors, base_dir);
    trace.borrow_mut().mark("markdown_parsed");

    let physical = Host::client_size(&window);
    let scale = Host::scale(&window);
    renderer.resize(scale);
    renderer.relayout((physical.0 as f32 / scale, physical.1 as f32 / scale));
    trace.borrow_mut().mark("laid_out");

    // Paint once before mapping so the window never flashes empty.
    renderer.paint(scale);
    let (pixels, w, h) = renderer.frame();
    Host::present(&window, pixels, w, h);
    Host::show(&window);
    {
        let mut trace = trace.borrow_mut();
        trace.mark("window_shown");
        trace.flush();
    }

    crate::render::drive(window, renderer)
}
