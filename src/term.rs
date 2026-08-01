use std::io::{self, IsTerminal, Read, Write};

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const MAGENTA: &str = "\x1b[35m";
pub const CYAN: &str = "\x1b[36m";
pub const BOLD_CYAN: &str = "\x1b[1;36m";
pub const BOLD_GREEN: &str = "\x1b[1;32m";

pub fn stdout_tty() -> bool {
    io::stdout().is_terminal()
}

pub fn stderr_tty() -> bool {
    io::stderr().is_terminal()
}

pub fn paint(s: &str, code: &str) -> String {
    if stdout_tty() {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint(s, BOLD)
}
pub fn dim(s: &str) -> String {
    paint(s, DIM)
}
pub fn green(s: &str) -> String {
    paint(s, GREEN)
}
pub fn bold_green(s: &str) -> String {
    paint(s, BOLD_GREEN)
}
pub fn yellow(s: &str) -> String {
    paint(s, YELLOW)
}
pub fn cyan(s: &str) -> String {
    paint(s, CYAN)
}
pub fn bold_cyan(s: &str) -> String {
    paint(s, BOLD_CYAN)
}
pub fn magenta(s: &str) -> String {
    paint(s, MAGENTA)
}

pub fn error(msg: &str) -> String {
    if stderr_tty() {
        format!("{RED}univ{RESET}: {msg}")
    } else {
        format!("univ: {msg}")
    }
}

pub fn warn(msg: &str) -> String {
    if stderr_tty() {
        format!("{YELLOW}univ{RESET}: {msg}")
    } else {
        format!("univ: {msg}")
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

pub struct Progress {
    label: String,
    total: Option<u64>,
    done: u64,
    frame: usize,
    shown: usize,
}

impl Progress {
    pub fn new(label: impl Into<String>, total: Option<u64>) -> Self {
        let mut p = Progress {
            label: label.into(),
            total,
            done: 0,
            frame: 0,
            shown: 0,
        };
        p.render();
        p
    }

    pub fn advance(&mut self, n: u64) {
        self.done += n;
        self.frame += 1;
        self.render();
    }

    pub fn finish(&mut self) {
        if stderr_tty() {
            eprintln!();
        }
        self.shown = 0;
    }

    fn render(&mut self) {
        if !stderr_tty() {
            return;
        }
        let width = term_width();
        let bar_len = 20usize;
        let label = truncate(&self.label, width.saturating_sub(bar_len + 30));

        let mut visible = label.len();
        let (body, body_visible) = match self.total {
            Some(total) if total > 0 => {
                let pct = ((self.done as f64 / total as f64) * 100.0).min(100.0);
                let filled = ((pct / 100.0) * bar_len as f64).round() as usize;
                let filled = filled.min(bar_len);
                let bar = format!(
                    "{GREEN}{}{RESET}{DIM}{}{RESET}",
                    "█".repeat(filled),
                    "░".repeat(bar_len - filled)
                );
                let pct = format!("{BOLD}{pct:>3.0}%{RESET}");
                let sizes_plain = format!("{} / {}", human_size(self.done), human_size(total));
                let sizes = format!("{DIM}{sizes_plain}{RESET}");
                let body = format!(" [{bar}] {pct} {sizes}");
                let body_visible = 1 + bar_len + 1 + 4 + 1 + sizes_plain.len();
                (body, body_visible)
            }
            _ => {
                let spin = SPINNER[self.frame % SPINNER.len()];
                let sizes_plain = human_size(self.done);
                let sizes = format!("{DIM}{sizes_plain}{RESET}");
                let body = format!(" {spin} {sizes}");
                let body_visible = 1 + 1 + 1 + sizes_plain.len();
                (body, body_visible)
            }
        };
        visible += body_visible;

        let line = format!("{CYAN}{label}{RESET}{body}");
        let mut out = format!("\r{line}");
        if visible < self.shown {
            out.push_str(&" ".repeat(self.shown - visible));
        }
        self.shown = visible;
        eprint!("{out}");
        let _ = io::stderr().flush();
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

pub fn http_get(url: &str, label: &str, max: Option<u64>) -> io::Result<Vec<u8>> {
    let mut res = match ureq::get(url).call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{url}: not found"),
            ));
        }
        Err(e) => return Err(io::Error::other(format!("{url}: {e}"))),
    };
    let total = res.body().content_length();
    let mut progress = Progress::new(label, total);
    let mut reader = res
        .body_mut()
        .with_config()
        .limit(max.unwrap_or(u64::MAX))
        .reader();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                progress.finish();
                return Err(io::Error::other(format!("{url}: {e}")));
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        progress.advance(n as u64);
    }
    progress.finish();
    Ok(buf)
}
