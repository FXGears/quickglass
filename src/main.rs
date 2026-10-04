#![windows_subsystem = "windows"]

mod native;

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

    // Anything that starts with `-` is ignored (including the old `--beta_render`)
    // so the first bare argument is always the file path.
    let mut file_arg: Option<String> = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
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

    native::run(
        &md_content,
        &title,
        file_arg.as_deref().map(std::path::Path::new),
        load_icon(),
        trace,
    );
}
