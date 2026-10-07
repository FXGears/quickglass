//! Minimal Win32 window and message loop.
//!
//! Replaces the `tao` crate, of which QuickGlass used one window, a handful of
//! input events, cursor shapes, and a timer. Raw Win32 messages are translated
//! into the small [`Ev`] enum and handed to a single handler closure.
//!
//! Behaviour carried over from tao: per-monitor-v2 DPI awareness, a dark title
//! bar when Windows apps use dark mode, mouse capture while a button is held,
//! and hidden-until-shown creation.

use std::cell::{Cell, RefCell};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH, ValidateRect};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
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
    KillTimer, LR_DEFAULTCOLOR, LoadCursorW, LoadImageW, MSG, RegisterClassExW, SPI_GETHIGHCONTRAST,
    SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SendMessageW, SetCursor, SetTimer, SetWindowPos, ShowWindow, SystemParametersInfoW,
    TranslateMessage, WINDOW_EX_STYLE, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETCURSOR, WM_SETICON, WM_SETTINGCHANGE, WM_SIZE, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

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

/// Input and window events, translated from Win32 messages.
pub(crate) enum Ev {
    Close,
    /// New client size in physical pixels.
    Resized(u32, u32),
    /// Cursor position in client DIPs; may lie outside the window while a
    /// button is held, because the mouse is captured.
    Moved(f32, f32),
    /// The cursor left the client area.
    Left,
    Button { button: Button, down: bool, at: (f32, f32) },
    /// Wheel notches, positive away from the user (fractional on fine wheels).
    Wheel(f32),
    /// A key press (including auto-repeat), as a Win32 virtual-key code.
    Key { vk: u16, ctrl: bool, shift: bool },
    /// A typed character, after keyboard-layout translation.
    Char(char),
    Timer,
    Paint,
}

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
/// a thread-local is dropped during `process::exit`, which releases the
/// Direct2D/DirectWrite objects it owns mid-teardown; measured, that stalled
/// exit by ~2 s (up to 10 s). Leaked, close-to-exit is 9 ms, as with tao.
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

    /// Native handle, for Direct2D and the clipboard.
    pub(crate) fn hwnd(&self) -> HWND {
        self.hwnd
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

    /// Runs the message loop, delivering every event to `handler`.
    ///
    /// Args:
    ///     handler: Called once per event on this thread.
    ///
    /// Returns:
    ///     Never; the process exits when the window closes.
    pub(crate) fn run(self, handler: impl FnMut(Ev) + 'static) -> ! {
        HANDLER.with(|slot| *slot.borrow_mut() = Some(Box::leak(Box::new(handler))));
        let mut msg = MSG::default();
        unsafe {
            // GetMessageW returns -1 on error, which is truthy, so test for > 0.
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::process::exit(0);
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
            // Direct2D does not validate the window itself; without this,
            // Windows would send WM_PAINT again immediately, forever.
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


