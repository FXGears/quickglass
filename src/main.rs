#![windows_subsystem = "windows"]

#[cfg(feature = "beta_render")]
mod native;

use std::os::windows::fs::OpenOptionsExt;

use pulldown_cmark::{Options, Parser, html};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::{WebContext, WebViewBuilder};

const CSS: &str = r#"
<style>
body {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
    font-size: 16px;
    line-height: 1.6;
    color: #e6edf3;
    background: #0d1117;
    max-width: 860px;
    margin: 0 auto;
    padding: 32px 24px;
}
::-webkit-scrollbar { width: 10px; background: #0d1117; }
::-webkit-scrollbar-thumb { background: #30363d; border-radius: 5px; }
::-webkit-scrollbar-thumb:hover { background: #484f58; }
html { background: #0d1117; }
h1, h2, h3, h4, h5, h6 { margin-top: 24px; margin-bottom: 16px; font-weight: 600; line-height: 1.25; color: #f0f6fc; }
h1 { font-size: 2em; padding-bottom: 0.3em; border-bottom: 1px solid #21262d; }
h2 { font-size: 1.5em; padding-bottom: 0.3em; border-bottom: 1px solid #21262d; }
h3 { font-size: 1.25em; }
code {
    background: #161b22;
    padding: 0.2em 0.4em;
    border-radius: 6px;
    font-size: 85%;
    font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace;
    color: #e6edf3;
}
pre {
    background: #161b22;
    padding: 16px;
    border-radius: 6px;
    overflow-x: auto;
    line-height: 1.45;
}
pre code { background: none; padding: 0; font-size: 85%; }
blockquote {
    margin: 0;
    padding: 0 1em;
    color: #8b949e;
    border-left: 0.25em solid #30363d;
}
table { border-collapse: collapse; width: 100%; margin: 16px 0; }
th, td { border: 1px solid #30363d; padding: 6px 13px; }
th { background: #161b22; font-weight: 600; }
tr:nth-child(even) { background: #161b22; }
a { color: #58a6ff; text-decoration: none; }
a:hover { text-decoration: underline; }
img { max-width: 100%; }
hr { border: none; border-top: 1px solid #21262d; margin: 24px 0; }
ul, ol { padding-left: 2em; }
li + li { margin-top: 0.5em; }
li { line-height: 1.7; }
strong { font-weight: 600; color: #f0f6fc; }
</style>
"#;

fn render_markdown(md_content: &str) -> String {
    let options = Options::all();
    let parser = Parser::new_ext(md_content, options);
    let mut html_output = String::with_capacity(md_content.len() * 2);
    html::push_html(&mut html_output, parser);

    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">{CSS}</head><body>{html_output}</body></html>"
    )
}

/// Opens a profile lock file with no sharing permitted.
///
/// A successful open means no other process holds the file, which is how a
/// running instance advertises ownership of its profile folder. The lock is
/// released by the OS when the owning process dies, so it survives crashes
/// and hard kills.
///
/// Args:
///     path: Path to the lock file, alongside the profile folder it guards.
///
/// Returns:
///     The open file handle on success, or `None` if another process holds it.
fn lock_profile(path: &std::path::Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .share_mode(0)
        .open(path)
        .ok()
}

/// Deletes WebView2 profile folders left behind by instances that have exited.
///
/// The process exits via `process::exit` without tearing down WebView2, so its
/// own profile folder is still locked at exit and cannot be removed then. It is
/// reclaimed here on a later launch instead. Folders whose lock file is still
/// held belong to a running instance and are left alone.
///
/// Args:
///     current: Name of this process's own profile folder, which is skipped.
fn sweep_orphan_profiles(current: &str) {
    let temp = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&temp) else {
        return;
    };

    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(pid) = name.strip_prefix("quickglass-") else {
            continue;
        };
        if name == current {
            continue;
        }

        // `quickglass-<pid>` is a profile folder; `quickglass-<pid>.lock` is its lock file.
        // Either entry is enough to identify the pair.
        let bare = pid.strip_suffix(".lock").unwrap_or(pid);
        if bare.is_empty() || !bare.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let dir = temp.join(format!("quickglass-{bare}"));
        let lock_path = temp.join(format!("quickglass-{bare}.lock"));

        if let Some(lock) = lock_profile(&lock_path) {
            let _ = std::fs::remove_dir_all(&dir);
            drop(lock);
            let _ = std::fs::remove_file(&lock_path);
        }
    }
}

/// Collects elapsed-time checkpoints across startup.
///
/// Inert unless `QUICKGLASS_TRACE` is set in the environment, so the release build
/// carries only one environment lookup and no I/O. When enabled, one line per
/// launch is appended to `%TEMP%\quickglass-startup.log` at the moment the window
/// becomes visible.
struct StartupTrace {
    start: std::time::Instant,
    marks: Vec<(&'static str, u128)>,
    enabled: bool,
}

impl StartupTrace {
    /// Starts the clock. Call as the first statement in `main`.
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            marks: Vec::new(),
            enabled: std::env::var_os("QUICKGLASS_TRACE").is_some(),
        }
    }

    /// Records microseconds elapsed since startup under `label`.
    ///
    /// Args:
    ///     label: Name of the startup phase that just completed.
    fn mark(&mut self, label: &'static str) {
        if self.enabled {
            self.marks.push((label, self.start.elapsed().as_micros()));
        }
    }

    /// Appends the collected checkpoints to the trace log as one line.
    fn flush(&self) {
        if !self.enabled {
            return;
        }
        use std::io::Write;
        let path = std::env::temp_dir().join("quickglass-startup.log");
        let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        let body: Vec<String> = self
            .marks
            .iter()
            .map(|(label, micros)| format!("{label}={:.2}ms", *micros as f64 / 1000.0))
            .collect();
        let _ = writeln!(file, "pid={} {}", std::process::id(), body.join(" "));
    }
}

/// Decodes the embedded PNG into a window icon.
///
/// Returns:
///     The icon used for the title bar, taskbar, and Alt-Tab.
fn load_icon() -> tao::window::Icon {
    let bytes = include_bytes!("../resources/icon-64.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("Failed to read icon PNG");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("Failed to decode icon");
    buf.truncate(info.buffer_size());
    tao::window::Icon::from_rgba(buf, info.width, info.height).expect("Failed to create icon")
}

fn main() {
    let trace = std::rc::Rc::new(std::cell::RefCell::new(StartupTrace::new()));

    // WebView2 renders a DefaultBackgroundColor (white by default) underneath web
    // content before any HTML is loaded, causing a white flash on startup. Set it to
    // match the page background (#0d1117) so launch is seamless. Must be set before
    // the WebView2 controller initializes.
    // SAFETY: called at the very start of main before any threads are spawned.
    unsafe {
        std::env::set_var("WEBVIEW2_DEFAULT_BACKGROUND_COLOR", "FF0D1117");
    }

    // `--beta_render` selects the native Direct2D renderer instead of WebView2.
    // Anything else that starts with `-` is ignored so the first bare argument
    // is always the file path.
    let mut beta_render = false;
    let mut file_arg: Option<String> = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--beta_render" => beta_render = true,
            _ if arg.starts_with('-') => {}
            _ if file_arg.is_none() => file_arg = Some(arg),
            _ => {}
        }
    }

    let md_content = match &file_arg {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) => format!("# Error\n\nCould not read file: `{path}`\n\n```\n{e}\n```"),
        },
        None => String::from("# QuickGlass\n\nNo file specified.\n\nUsage: `quickglass <file.md>`"),
    };

    let title = match &file_arg {
        Some(path) => {
            let path = std::path::Path::new(path);
            format!("{} — QuickGlass", path.file_name().unwrap_or_default().to_string_lossy())
        }
        None => String::from("QuickGlass"),
    };

    trace.borrow_mut().mark("file_read");

    if beta_render {
        // The native path needs no WebView2 profile, so it skips all of the
        // profile and lock handling below. In a build without the `beta_render`
        // feature this block is empty, so `--beta_render` is accepted and ignored
        // and the WebView2 path runs as usual.
        #[cfg(feature = "beta_render")]
        native::run(&md_content, &title, load_icon(), trace.clone());
    }

    let html = render_markdown(&md_content);
    trace.borrow_mut().mark("markdown_rendered");

    // Use a per-process WebView2 profile under %TEMP% instead of a persistent one in
    // %LOCALAPPDATA%. The persistent profile grew to ~30 MB and kept regrowing every
    // run, dominating the on-disk footprint. A per-process folder is reclaimed by the
    // next launch, so nothing accumulates across runs.
    let profile_name = format!("quickglass-{}", std::process::id());
    let temp = std::env::temp_dir();
    let data_dir = temp.join(&profile_name);
    // Held for the lifetime of the process so other instances leave this profile alone.
    let _profile_lock = lock_profile(&temp.join(format!("{profile_name}.lock")));
    // Off-thread so enumerating %TEMP% never delays the window appearing.
    std::thread::spawn(move || sweep_orphan_profiles(&profile_name));

    trace.borrow_mut().mark("profile_ready");

    let mut web_context = WebContext::new(Some(data_dir));
    trace.borrow_mut().mark("web_context");

    let event_loop = EventLoop::new();
    trace.borrow_mut().mark("event_loop");
    let icon = load_icon();
    let window = WindowBuilder::new()
        .with_title(&title)
        .with_window_icon(Some(icon))
        .with_background_color((13, 17, 23, 255))
        .with_visible(false)
        .with_inner_size(tao::dpi::LogicalSize::new(920.0, 700.0))
        .build(&event_loop)
        .expect("Failed to create window");
    let window = std::rc::Rc::new(window);
    trace.borrow_mut().mark("window_created");

    // Defer showing the window until the page has finished rendering. WebView2 paints
    // a white surface underneath content before the HTML loads; keeping the window
    // hidden until PageLoadEvent::Finished means the window only ever appears with
    // content already painted, eliminating the startup white flash.
    let window_for_load = window.clone();
    let trace_for_load = trace.clone();
    let _webview = WebViewBuilder::with_web_context(&mut web_context)
        .with_background_color((13, 17, 23, 255))
        .with_on_page_load_handler(move |event, _url| {
            if let wry::PageLoadEvent::Finished = event {
                window_for_load.set_visible(true);
                let mut trace = trace_for_load.borrow_mut();
                trace.mark("window_shown");
                trace.flush();
            }
        })
        .with_html(&html)
        .build(&*window)
        .expect("Failed to create webview");
    trace.borrow_mut().mark("webview_built");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            let mut trace = trace.borrow_mut();
            trace.mark("close_requested");
            trace.flush();
            // Exit here rather than via ControlFlow::Exit. tao only tests the exit flag
            // after GetMessageW returns another message, and closing the window produces
            // none of its own, so the pump can block indefinitely with the process still
            // alive and holding its WebView2 profile. Exiting directly is immediate and
            // deterministic; the profile folder is reclaimed by a later launch in
            // sweep_orphan_profiles.
            std::process::exit(0);
        }
    });
}
