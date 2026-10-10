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

/// How many colors the terminal can show (`NO_COLOR` / non-TTY are handled
/// by [`color_enabled`]; this only picks the palette).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Basic,
    Xterm256,
    TrueColor,
}

impl Depth {
    pub fn from_env(colorterm: Option<&str>, term: Option<&str>) -> Depth {
        if colorterm
            .is_some_and(|c| c.eq_ignore_ascii_case("truecolor") || c.eq_ignore_ascii_case("24bit"))
        {
            Depth::TrueColor
        } else if term.is_some_and(|t| t.contains("256color")) {
            Depth::Xterm256
        } else {
            Depth::Basic
        }
    }

    fn current() -> Depth {
        static D: std::sync::OnceLock<Depth> = std::sync::OnceLock::new();
        *D.get_or_init(|| {
            Depth::from_env(
                std::env::var("COLORTERM").ok().as_deref(),
                std::env::var("TERM").ok().as_deref(),
            )
        })
    }
}

/// The brand color of a safety tier (lysosome-dev/brand, `color.tier.*`):
/// regenerable = GFP mint `#6ef0b0`, expensive = amber `#ffc15e`, review =
/// DAPI blue-violet `#92a9ff`. Exact on truecolor terminals, the nearest
/// xterm-256 color on 256-color ones (121, 215, 111), else green / yellow /
/// blue.
pub fn tier_style(t: lysosome_core::model::Tier) -> Style {
    tier_style_at(t, Depth::current())
}

pub fn tier_style_at(t: lysosome_core::model::Tier, d: Depth) -> Style {
    use lysosome_core::model::Tier;
    use owo_colors::{AnsiColors, XtermColors};
    let (rgb, xterm, basic) = match t {
        Tier::Regenerable => ((0x6e, 0xf0, 0xb0), 121u8, AnsiColors::Green),
        Tier::RegenerableExpensive => ((0xff, 0xc1, 0x5e), 215, AnsiColors::Yellow),
        Tier::Review => ((0x92, 0xa9, 0xff), 111, AnsiColors::Blue),
    };
    match d {
        Depth::TrueColor => Style::new().truecolor(rgb.0, rgb.1, rgb.2),
        Depth::Xterm256 => Style::new().color(XtermColors::from(xterm)),
        Depth::Basic => Style::new().color(basic),
    }
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

/// Quote an argument for a POSIX shell. Double quotes like the prototype,
/// unless the text contains `!` (history expansion in interactive bash),
/// in which case single quotes are used.
pub fn shell_quote(s: &str) -> String {
    if s.contains('!') {
        return format!("'{}'", s.replace('\'', "'\\''"));
    }
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
        assert_eq!(shell_quote("/wow!/it's"), "'/wow!/it'\\''s'");
    }

    #[test]
    fn tier_colors_follow_the_brand_palette() {
        use lysosome_core::model::Tier;
        assert_eq!(
            Depth::from_env(Some("truecolor"), Some("xterm")),
            Depth::TrueColor
        );
        assert_eq!(
            Depth::from_env(None, Some("xterm-256color")),
            Depth::Xterm256
        );
        assert_eq!(Depth::from_env(None, Some("xterm")), Depth::Basic);
        let p = Painter::new(true);
        let s = |t, d| p.paint("x", tier_style_at(t, d));
        assert_eq!(
            s(Tier::Regenerable, Depth::TrueColor),
            "\x1b[38;2;110;240;176mx\x1b[0m"
        );
        assert_eq!(
            s(Tier::RegenerableExpensive, Depth::Xterm256),
            "\x1b[38;5;215mx\x1b[0m"
        );
        assert_eq!(s(Tier::Review, Depth::Basic), "\x1b[34mx\x1b[0m");
        // NO_COLOR / non-TTY: plain.
        assert_eq!(
            Painter::new(false).paint("x", tier_style(Tier::Review)),
            "x"
        );
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("feature/very-long-branch", 10), "feature/v…");
        assert_eq!(truncate("main", 10), "main");
    }
}
