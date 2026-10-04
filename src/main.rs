#![windows_subsystem = "windows"]

mod native;

/// Collects elapsed-time checkpoints across startup.
///
/// Compiled in only with `--features trace`. In a normal build the struct is
/// empty and every method is a no-op, so the shipped binary carries no timing,
/// formatting, or file I/O code. In a trace build, one line per launch is
/// appended to `%TEMP%\quickglass-startup.log` when the window becomes visible,
/// with each phase as whole microseconds since process start.
struct StartupTrace {
    #[cfg(feature = "trace")]
    start: std::time::Instant,
    #[cfg(feature = "trace")]
    marks: Vec<(&'static str, u128)>,
}

impl StartupTrace {
    /// Starts the clock. Call as the first statement in `main`.
    fn new() -> Self {
        Self {
            #[cfg(feature = "trace")]
            start: std::time::Instant::now(),
            #[cfg(feature = "trace")]
            marks: Vec::new(),
        }
    }

    /// Records microseconds elapsed since startup under `label`.
    ///
    /// Args:
    ///     label: Name of the startup phase that just completed.
    #[cfg_attr(not(feature = "trace"), allow(unused_variables))]
    fn mark(&mut self, label: &'static str) {
        #[cfg(feature = "trace")]
        self.marks.push((label, self.start.elapsed().as_micros()));
    }

    /// Appends the collected checkpoints to the trace log as one line.
    fn flush(&self) {
        #[cfg(feature = "trace")]
        {
            use std::io::Write;
            let path = std::env::temp_dir().join("quickglass-startup.log");
            let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path)
            else {
                return;
            };
            let body: Vec<String> =
                self.marks.iter().map(|(label, micros)| format!("{label}_us={micros}")).collect();
            let _ = writeln!(file, "pid={} {}", std::process::id(), body.join(" "));
        }
    }
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
        trace,
    );
}
