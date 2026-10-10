//! Presentation helpers: color policy, sizes, numbers, paths.

use std::fmt::Display;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use owo_colors::{OwoColorize, Style};

/// Decide whether a stream gets color: not disabled by `--no-color` or
/// `NO_COLOR`, attached to a terminal, and not a dumb terminal.
pub fn color_enabled(no_color_flag: bool, is_terminal: bool) -> bool {
    if no_color_flag || std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("TERM").is_some_and(|t| t == "dumb") {
        return false;
    }
    is_terminal
}

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// Applies styles only when color is on.
#[derive(Debug, Clone, Copy)]
pub struct Painter {
    on: bool,
}

impl Painter {
    pub fn new(on: bool) -> Self {
        Painter { on }
    }

    pub fn paint(&self, s: impl Display, style: Style) -> String {
        if self.on {
            s.style(style).to_string()
        } else {
            s.to_string()
        }
    }

    pub fn bold(&self, s: impl Display) -> String {
        self.paint(s, Style::new().bold())
    }

    pub fn dim(&self, s: impl Display) -> String {
        self.paint(s, Style::new().dimmed())
    }
}

/// Human-readable size using SI units (1 GB = 10^9 bytes), matching Finder.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut unit = UNITS[0];
    for u in &UNITS[1..] {
        if v < 999.95 {
            break;
        }
        v /= 1000.0;
        unit = u;
    }
    if v < 99.95 {
        format!("{v:.1} {unit}")
    } else {
        format!("{v:.0} {unit}")
    }
}

/// `1234567` -> `1,234,567`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `n thing` / `n things`.
pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

/// Replaces the home-directory prefix with `~` for display.
#[derive(Debug, Clone, Default)]
pub struct PathShortener {
    homes: Vec<PathBuf>,
}

impl PathShortener {
    pub fn new(home: Option<PathBuf>) -> Self {
        let mut homes = Vec::new();
        if let Some(h) = home {
            // Scanned paths are canonical; HOME may not be (e.g. /var vs /private/var).
            if let Ok(c) = h.canonicalize() {
                if c != h {
                    homes.push(c);
                }
            }
            homes.push(h);
        }
        PathShortener { homes }
    }

    pub fn short(&self, p: &Path) -> String {
        for h in &self.homes {
            if let Ok(rest) = p.strip_prefix(h) {
                if rest.as_os_str().is_empty() {
                    return "~".into();
                }
                return format!("~/{}", rest.display());
            }
        }
        p.display().to_string()
    }
}

/// Truncate to `max` chars, ending with `…` when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Pad `s` (whose visible width is `width_of(s)`) to `w` columns.
pub fn pad_right(s: &str, w: usize) -> String {
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(w.saturating_sub(n)))
}

pub fn pad_left(s: &str, w: usize) -> String {
    let n = s.chars().count();
    format!("{}{s}", " ".repeat(w.saturating_sub(n)))
}

/// Quote a path for a POSIX shell command line (double quotes, like the prototype).
pub fn shell_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanizes_sizes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_000), "1.0 KB");
        assert_eq!(human_bytes(20 * 1024 * 1024), "21.0 MB");
        assert_eq!(human_bytes(812_000_000), "812 MB");
        assert_eq!(human_bytes(999_960_000), "1.0 GB");
        assert_eq!(human_bytes(4_100_000_000), "4.1 GB");
        assert_eq!(human_bytes(2_500_000_000_000), "2.5 TB");
    }

    #[test]
    fn formats_numbers_and_plurals() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(plural(1, "folder", "folders"), "1 folder");
        assert_eq!(plural(1200, "folder", "folders"), "1,200 folders");
    }

    #[test]
    fn shortens_home_paths() {
        let s = PathShortener::new(Some("/Users/me".into()));
        assert_eq!(s.short(Path::new("/Users/me/code/x")), "~/code/x");
        assert_eq!(s.short(Path::new("/Users/me")), "~");
        assert_eq!(s.short(Path::new("/Users/meow")), "/Users/meow");
    }

    #[test]
    fn quotes_for_shell() {
        assert_eq!(shell_quote("/a b/c"), "\"/a b/c\"");
        assert_eq!(shell_quote("/a$b\"c"), "\"/a\\$b\\\"c\"");
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("feature/very-long-branch", 10), "feature/v…");
        assert_eq!(truncate("main", 10), "main");
    }
}
