//! Minimal Win32 window, message loop, and software blit (Windows).
//!
//! This is the Windows shell for the shared renderer in [`crate::render`]. It
//! owns a single top-level window, translates raw Win32 messages into the small
//! [`Ev`] enum the renderer consumes, and presents CPU-rendered frames with a
//! top-down 32-bit BGRA DIB (`SetDIBitsToDevice`). The renderer produces
//! `0xAARRGGBB` words, which on little-endian Windows are BGRA byte order, so
//! the frame blits without conversion.
//!
//! Behaviour carried over from the original tao/Direct2D build: per-monitor-v2
//! DPI awareness, a dark title bar when Windows apps use dark mode, mouse
//! capture while a button is held, hidden-until-shown creation, and
//! CF_UNICODETEXT clipboard copy.
//!
//! The window implements [`crate::render::Host`] (size, scale, present, cursor,
//! timer, selection, message loop) and [`crate::render::FontSource`] (the four
//! face files, resolved from the Windows font registry). Everything visual —
//! layout, paint, hit-testing, search, selection — lives in [`crate::render`].

use std::cell::{Cell, RefCell};

use windows::Win32::Foundation::{
    COLORREF, E_OUTOFMEMORY, GlobalFree, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateSolidBrush, DIB_RGB_COLORS, GetDC, HBRUSH,
    ReleaseDC, SetDIBitsToDevice, ValidateRect,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_VALUE_TYPE, RRF_RT_REG_DWORD,
    RegCloseKey, RegGetValueW, RegOpenKeyExW, RegQueryValueExW,
};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
    HTCLIENT, ICON_BIG, ICON_SMALL, IDC_ARROW, IDC_HAND, IDC_IBEAM, IDC_SIZENS, IMAGE_ICON,
    KillTimer, LR_DEFAULTCOLOR, LoadCursorW, LoadImageW, MB_ICONERROR, MB_OK, MSG, MessageBoxW,
    RegisterClassExW, SPI_GETHIGHCONTRAST,
    SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SendMessageW, SetCursor, SetTimer, SetWindowPos, ShowWindow, SystemParametersInfoW,
    TranslateMessage, WINDOW_EX_STYLE, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETICON,
    WM_SETTINGCHANGE, WM_SIZE, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

use crate::render::{Button, Cursor, Ev, FaceKind, FontSource, Host, Renderer};

/// Mouse-button bits in a mouse message's `wParam` (`MK_LBUTTON` etc.).
const MK_ANY_BUTTON: usize = 0x0001 | 0x0002 | 0x0010 | 0x0020 | 0x0040;
/// Defined in the windows crate's large `UI_Controls` module; declared here
/// rather than enabling that whole feature for one constant.
const WM_MOUSELEAVE: u32 = 0x02A3;
/// The only timer the app uses: autoscroll steps.
const TIMER_ID: usize = 1;
/// Resource ID of the application icon; `build.rs` embeds `icon.ico` as ID 1.
const ICON_RESOURCE: u16 = 1;
/// Page background #0d1117 as a GDI COLORREF (0x00BBGGRR).
const BACKGROUND: COLORREF = COLORREF(0x0017_110D);

/// The event handler, deliberately leaked rather than boxed. A boxed handler in
/// a thread-local is dropped during `process::exit`, which would tear down the
/// renderer's FreeType library and glyph cache it owns mid-teardown; leaking it
/// keeps close-to-exit fast and avoids freeing live font state on the way out.
type Handler = &'static mut dyn FnMut(Ev);

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
    /// Kept outside the handler so WM_SETCURSOR can always read it, even when
    /// it arrives while the handler is running.
    static CURSOR: Cell<Cursor> = const { Cell::new(Cursor::Arrow) };
    static TRACKING_LEAVE: Cell<bool> = const { Cell::new(false) };
    static HIGH_SURROGATE: Cell<u16> = const { Cell::new(0) };
}

/// The application window. A plain handle, so it is freely copyable.
#[derive(Clone, Copy)]
pub(crate) struct Window {
    hwnd: HWND,
}

impl Window {
    /// Creates the window hidden, with a client area of `size` DIPs.
    ///
    /// Args:
    ///     title: Window title.
    ///     size: Client width and height in DIPs at the window's DPI.
    ///
    /// Raises:
    ///     Returns the Win32 error if the class or window cannot be created.
    pub(crate) fn create(title: &str, size: (f32, f32)) -> windows::core::Result<Self> {
        unsafe {
            // Per-monitor v2 keeps text sharp at every scale factor; fall back to
            // v1 on older Windows 10 builds, as tao did.
            if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
                let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
            }

            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH(CreateSolidBrush(BACKGROUND).0),
                lpszClassName: w!("QuickGlass"),
                ..Default::default()
            };
            RegisterClassExW(&class);

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("QuickGlass"),
                &HSTRING::from(title),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            let window = Window { hwnd };

            // The DPI is only known once the window exists on a monitor, so size
            // it afterwards; it is still hidden, so nothing visible happens.
            let dpi = GetDpiForWindow(hwnd);
            let scale = dpi as f32 / 96.0;
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: (size.0 * scale).round() as i32,
                bottom: (size.1 * scale).round() as i32,
            };
            AdjustWindowRectExForDpi(&mut rect, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE::default(), dpi)?;
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )?;

            window.set_icons(scale);
            apply_title_bar_theme(hwnd);
            Ok(window)
        }
    }

    /// Physical pixels per DIP for the monitor the window is on.
    pub(crate) fn scale(&self) -> f32 {
        unsafe { GetDpiForWindow(self.hwnd) as f32 / 96.0 }
    }

    /// Client area size in physical pixels.
    pub(crate) fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rect);
        }
        ((rect.right - rect.left).max(0) as u32, (rect.bottom - rect.top).max(0) as u32)
    }

    pub(crate) fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
        }
    }

    /// Sets the cursor shown over the client area, taking effect immediately.
    pub(crate) fn set_cursor(&self, cursor: Cursor) {
        CURSOR.with(|current| current.set(cursor));
        apply_cursor(cursor);
    }

    /// Starts the repeating timer that delivers [`Ev::Timer`].
    pub(crate) fn start_timer(&self, interval_ms: u32) {
        unsafe {
            SetTimer(Some(self.hwnd), TIMER_ID, interval_ms, None);
        }
    }

    pub(crate) fn stop_timer(&self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_ID);
        }
    }

    /// Loads the embedded icon at the sizes this display wants: 16 DIPs for
    /// the title bar, 32 for the taskbar and Alt-Tab.
    fn set_icons(&self, scale: f32) {
        for (kind, dips) in [(ICON_SMALL, 16.0), (ICON_BIG, 32.0)] {
            let px = (dips * scale).round() as i32;
            unsafe {
                let instance = GetModuleHandleW(None).ok().map(Into::into);
                let name = PCWSTR(ICON_RESOURCE as usize as *const u16);
                if let Ok(icon) = LoadImageW(instance, name, IMAGE_ICON, px, px, LR_DEFAULTCOLOR) {
                    SendMessageW(
                        self.hwnd,
                        WM_SETICON,
                        Some(WPARAM(kind as usize)),
                        Some(LPARAM(icon.0 as isize)),
                    );
                }
            }
        }
    }

    /// Uploads a frame to the window as a top-down 32-bit BGRA DIB.
    ///
    /// The renderer produces `0xAARRGGBB` words; on little-endian Windows that
    /// memory reads as `[B, G, R, A]`, which is exactly the byte order a
    /// `BI_RGB` 32-bpp DIB expects, so the words blit without conversion. A
    /// negative `biHeight` makes the DIB top-down to match the renderer's
    /// row 0 = top convention.
    ///
    /// Args:
    ///     pixels: Frame as packed `0xAARRGGBB` words, `w * h` long.
    ///     w: Frame width in pixels.
    ///     h: Frame height in pixels.
    pub(crate) fn present(&self, pixels: &[u32], w: u32, h: u32) {
        if w == 0 || h == 0 || pixels.len() < (w as usize) * (h as usize) {
            return;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                // Negative height: top-down rows, matching the renderer.
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: a device context for the client area is fetched and released
        // on every path; `pixels` holds at least `w*h` words, matching the DIB
        // dimensions and 32-bpp format passed to SetDIBitsToDevice.
        unsafe {
            let hdc = GetDC(Some(self.hwnd));
            if hdc.is_invalid() {
                return;
            }
            SetDIBitsToDevice(
                hdc,
                0,
                0,
                w,
                h,
                0,
                0,
                0,
                h,
                pixels.as_ptr() as *const core::ffi::c_void,
                &info,
                DIB_RGB_COLORS,
            );
            ReleaseDC(Some(self.hwnd), hdc);
        }
    }

    /// Copies `text` to the clipboard as CF_UNICODETEXT.
    ///
    /// Newlines are converted to CRLF, which is what Windows applications
    /// expect when pasting. Any Win32 failure is swallowed, matching the
    /// renderer's best-effort selection contract.
    ///
    /// Args:
    ///     text: Text to copy.
    pub(crate) fn set_selection(&self, text: &str) {
        let _ = copy_to_clipboard(self.hwnd, text);
    }
}

/// Places text on the clipboard as Unicode text.
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
    // SAFETY: the clipboard is opened and closed on every path; on success the
    // clipboard takes ownership of the global memory, on failure it is freed.
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

/// Shows `cursor` now.
fn apply_cursor(cursor: Cursor) {
    let id = match cursor {
        Cursor::Arrow => IDC_ARROW,
        Cursor::Hand => IDC_HAND,
        Cursor::IBeam => IDC_IBEAM,
        Cursor::SizeNs => IDC_SIZENS,
    };
    unsafe {
        if let Ok(handle) = LoadCursorW(None, id) {
            SetCursor(Some(handle));
        }
    }
}

/// Uses a dark title bar when Windows apps are set to dark mode and high
/// contrast is off, matching what tao did.
fn apply_title_bar_theme(hwnd: HWND) {
    let dark = BOOL::from(apps_use_dark_mode() && !high_contrast());
    let size = std::mem::size_of::<BOOL>() as u32;
    let value = &dark as *const BOOL as *const core::ffi::c_void;
    unsafe {
        // Attribute 20 since Windows 10 20H1; 19 on the builds just before it.
        if DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(20), value, size).is_err() {
            let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(19), value, size);
        }
    }
}

/// The documented per-user "Choose your app mode" setting.
fn apps_use_dark_mode() -> bool {
    let mut light: u32 = 1;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut light as *mut u32 as *mut core::ffi::c_void),
            Some(&mut bytes),
        )
    };
    status.is_ok() && light == 0
}

fn high_contrast() -> bool {
    let mut info = HIGHCONTRASTW { cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            info.cbSize,
            Some(&mut info as *mut HIGHCONTRASTW as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_ok() && (info.dwFlags & HCF_HIGHCONTRASTON).0 != 0
}

/// Hands an event to the handler.
///
/// Returns:
///     False when the handler is already running (a message re-entered the
///     window procedure from inside it), so the caller falls back to the
///     default processing instead of panicking on the RefCell.
fn dispatch(ev: Ev) -> bool {
    HANDLER.with(|slot| match slot.try_borrow_mut() {
        Ok(mut handler) => match handler.as_mut() {
            Some(handler) => {
                handler(ev);
                true
            }
            None => false,
        },
        Err(_) => false,
    })
}

/// Client coordinates of a mouse message, in DIPs. Signed: negative while the
/// mouse is captured and the cursor is left of or above the window.
fn mouse_point(hwnd: HWND, lparam: LPARAM) -> (f32, f32) {
    let x = (lparam.0 & 0xFFFF) as u16 as i16 as f32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    (x / scale, y / scale)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let button = |button: Button, down: bool| {
        unsafe {
            if down {
                // Keep receiving moves while dragging, even outside the window.
                SetCapture(hwnd);
            } else if wparam.0 & MK_ANY_BUTTON == 0 {
                let _ = ReleaseCapture();
            }
        }
        Some(Ev::Button { button, down, at: mouse_point(hwnd, lparam) })
    };

    let ev = match msg {
        WM_CLOSE => Some(Ev::Close),
        // Reached only if the handler was busy when WM_CLOSE arrived and the
        // default processing destroyed the window; never outlive the window.
        WM_DESTROY => std::process::exit(0),
        WM_SIZE => {
            const SIZE_MINIMIZED: usize = 1;
            if wparam.0 == SIZE_MINIMIZED {
                None
            } else {
                Some(Ev::Resized((lparam.0 & 0xFFFF) as u32, ((lparam.0 >> 16) & 0xFFFF) as u32))
            }
        }
        WM_MOUSEMOVE => {
            if !TRACKING_LEAVE.with(Cell::get) {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                if unsafe { TrackMouseEvent(&mut track) }.is_ok() {
                    TRACKING_LEAVE.with(|t| t.set(true));
                }
            }
            let (x, y) = mouse_point(hwnd, lparam);
            Some(Ev::Moved(x, y))
        }
        WM_MOUSELEAVE => {
            TRACKING_LEAVE.with(|t| t.set(false));
            Some(Ev::Left)
        }
        WM_LBUTTONDOWN => button(Button::Left, true),
        WM_LBUTTONUP => button(Button::Left, false),
        WM_MBUTTONDOWN => button(Button::Middle, true),
        WM_MBUTTONUP => button(Button::Middle, false),
        WM_RBUTTONDOWN | WM_XBUTTONDOWN => button(Button::Other, true),
        WM_RBUTTONUP | WM_XBUTTONUP => button(Button::Other, false),
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
            Some(Ev::Wheel(f32::from(delta) / 120.0))
        }
        WM_KEYDOWN => {
            let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| unsafe {
                GetKeyState(i32::from(vk.0)) < 0
            };
            Some(Ev::Key { vk: wparam.0 as u16, ctrl: down(VK_CONTROL), shift: down(VK_SHIFT) })
        }
        WM_CHAR => {
            // WM_CHAR carries UTF-16 units; characters outside the BMP arrive as
            // a surrogate pair in two messages.
            let unit = wparam.0 as u16;
            match unit {
                0xD800..=0xDBFF => {
                    HIGH_SURROGATE.with(|h| h.set(unit));
                    None
                }
                0xDC00..=0xDFFF => {
                    let high = HIGH_SURROGATE.with(|h| h.replace(0));
                    char::decode_utf16([high, unit]).next().and_then(Result::ok).map(Ev::Char)
                }
                _ => char::from_u32(u32::from(unit)).map(Ev::Char),
            }
        }
        WM_TIMER if wparam.0 == TIMER_ID => Some(Ev::Timer),
        WM_PAINT => {
            dispatch(Ev::Paint);
            // The present path blits directly and does not validate the window
            // itself; without this, Windows would send WM_PAINT again
            // immediately, forever.
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            return LRESULT(0);
        }
        // Every frame repaints the whole client area, so erasing first would
        // only add flicker.
        WM_ERASEBKGND => return LRESULT(1),
        WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
            apply_cursor(CURSOR.with(Cell::get));
            return LRESULT(1);
        }
        WM_DPICHANGED => {
            // Adopt the size Windows suggests for the new monitor's DPI; the
            // resulting WM_SIZE relays out the document.
            let suggested = unsafe { &*(lparam.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            Window { hwnd }.set_icons(unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0);
            return LRESULT(0);
        }
        WM_SETTINGCHANGE => {
            // Follows the app-mode setting if it changes while open.
            apply_title_bar_theme(hwnd);
            None
        }
        _ => None,
    };

    match ev {
        Some(ev) => {
            if dispatch(ev) {
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}



// ---------------------------------------------------------------------------
// Host + FontSource — the two contracts the shared renderer drives the shell
// through. The window methods are inherent (above); these impls expose them to
// `crate::render` and add the Win32 message loop and registry font resolution.
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

    /// Runs the Win32 message loop, delivering every event to `handler`.
    ///
    /// `Window` is a plain copyable handle, so a copy is captured for the
    /// borrow the shared loop expects; the adapter closure is leaked (see
    /// [`Handler`]) rather than dropped at exit.
    ///
    /// Args:
    ///     handler: Called once per event on this thread.
    ///
    /// Returns:
    ///     Never; the process exits when the window closes.
    fn run_loop(self, mut handler: impl FnMut(&Self, Ev) + 'static) -> ! {
        let window = self;
        let adapter = move |ev: Ev| handler(&window, ev);
        HANDLER.with(|slot| *slot.borrow_mut() = Some(Box::leak(Box::new(adapter))));
        let mut msg = MSG::default();
        // SAFETY: a standard Win32 message pump; GetMessageW returns -1 on
        // error, which is truthy, so the loop tests for > 0.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::process::exit(0);
    }
}

/// Registry-backed font resolution for the Windows shell.
///
/// Resolves each display face to a `.ttf`/`.ttc` under `%WINDIR%\Fonts`, with
/// no Fontconfig. The known Segoe UI / Consolas file names are used as a robust
/// default, and the font registry
/// (`HKLM\...\Fonts`, then `HKCU\...\Fonts`) is consulted as a fallback when a
/// default file is missing (e.g. renamed by the OEM).
struct RegistryFontSource;

impl FontSource for RegistryFontSource {
    fn resolve(&self, kind: FaceKind) -> Option<(String, u32)> {
        // (default file name under %WINDIR%\Fonts, registry value name).
        let (default_file, registry_name) = match kind {
            FaceKind::Sans => ("segoeui.ttf", "Segoe UI (TrueType)"),
            FaceKind::SansBold => ("segoeuib.ttf", "Segoe UI Bold (TrueType)"),
            FaceKind::SansItalic => ("segoeuii.ttf", "Segoe UI Italic (TrueType)"),
            FaceKind::Mono => ("consola.ttf", "Consolas (TrueType)"),
        };

        let fonts_dir = fonts_directory();
        let default_path = format!("{fonts_dir}\\{default_file}");
        if std::path::Path::new(&default_path).is_file() {
            return Some((default_path, 0));
        }

        // Fallback: look the display name up in the font registry. The value is
        // usually a bare file name resolved against %WINDIR%\Fonts, but may be
        // an absolute path.
        let file = registry_font_file(registry_name)?;
        let path = if std::path::Path::new(&file).is_absolute() {
            file
        } else {
            format!("{fonts_dir}\\{file}")
        };
        std::path::Path::new(&path).is_file().then_some((path, 0))
    }
}

/// The system Fonts directory, e.g. `C:\Windows\Fonts`.
///
/// Returns:
///     `%WINDIR%\Fonts`, falling back to `C:\Windows\Fonts` when `WINDIR` is
///     unset.
fn fonts_directory() -> String {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| String::from("C:\\Windows"));
    format!("{windir}\\Fonts")
}

/// Reads a font file name from the Windows font registry.
///
/// Tries the per-machine key first, then the per-user key, under
/// `Software\Microsoft\Windows NT\CurrentVersion\Fonts`.
///
/// Args:
///     name: The registry value name, e.g. `"Segoe UI (TrueType)"`.
///
/// Returns:
///     The stored file name or path, or `None` if the value is absent.
fn registry_font_file(name: &str) -> Option<String> {
    const SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows NT\\CurrentVersion\\Fonts");
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        if let Some(value) = read_registry_string(root, SUBKEY, name) {
            return Some(value);
        }
    }
    None
}

/// Reads a single `REG_SZ` value from the registry as a `String`.
///
/// Args:
///     root: Registry root key (HKLM or HKCU).
///     subkey: Path of the subkey to open.
///     value_name: Name of the value to read.
///
/// Returns:
///     The string value, or `None` on any failure or wrong type.
fn read_registry_string(root: HKEY, subkey: PCWSTR, value_name: &str) -> Option<String> {
    let name: Vec<u16> = value_name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: the key is opened and closed on every path; the two-call size
    // protocol is followed, and the buffer is sized from the first call's
    // reported byte count before the second call fills it.
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(root, subkey, Some(0), KEY_READ, &mut key).is_err() {
            return None;
        }
        let result = (|| {
            let mut kind = REG_VALUE_TYPE::default();
            let mut bytes: u32 = 0;
            // First call: learn the byte length (including the trailing NUL).
            if RegQueryValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                None,
                Some(&mut bytes),
            )
            .is_err()
                || bytes == 0
            {
                return None;
            }
            let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
            if RegQueryValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(buffer.as_mut_ptr() as *mut u8),
                Some(&mut bytes),
            )
            .is_err()
            {
                return None;
            }
            // Trim the trailing NUL (and anything past it) before decoding.
            let len = buffer.iter().position(|&u| u == 0).unwrap_or(buffer.len());
            Some(String::from_utf16_lossy(&buffer[..len]))
        })();
        let _ = RegCloseKey(key);
        result
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Shows a startup error in a message box and exits.
///
/// The GUI subsystem has no console, so a panic message would be invisible.
///
/// Args:
///     message: Error text shown to the user.
fn fatal(message: &str) -> ! {
    // SAFETY: HSTRINGs outlive the call; no owner window is required.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(format!("QuickGlass could not start:\n\n{message}")),
            &HSTRING::from("QuickGlass"),
            MB_OK | MB_ICONERROR,
        );
    }
    std::process::exit(1)
}


/// Opens a window and renders `source` with the shared CPU renderer until
/// closed.
///
/// It creates the window, builds the renderer with registry-resolved faces,
/// parses the markdown, lays it out, paints once before showing, then hands the
/// window and renderer to the shared [`crate::render::drive`] loop. The
/// [`crate::StartupTrace`] marks match the Linux shell so both report on one
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
    path: Option<&std::path::Path>,
    trace: std::rc::Rc<std::cell::RefCell<crate::StartupTrace>>,
) -> ! {
    let base_dir = path.and_then(std::path::Path::parent).map(std::path::Path::to_path_buf);
    // Created hidden; icons, DPI awareness and title-bar theme are set inside.
    let window = Window::create(title, (920.0, 700.0)).unwrap_or_else(|e| fatal(&e.to_string()));
    trace.borrow_mut().mark("window_created");

    let mut renderer = Renderer::new(&RegistryFontSource).unwrap_or_else(|e| fatal(&e));
    trace.borrow_mut().mark("renderer_ready");

    let (blocks, anchors) = crate::doc::parse(source);
    renderer.set_document(blocks, anchors, base_dir);
    trace.borrow_mut().mark("markdown_parsed");

    let physical = Host::client_size(&window);
    let scale = Host::scale(&window);
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
