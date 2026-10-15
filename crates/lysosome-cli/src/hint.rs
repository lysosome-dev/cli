//! One short, dim line after a human-readable `scan` or `clean` that points
//! at the desktop app. It is only ever shown to a person at a terminal:
//! never with `--json`, never with `--progress json`, never when stdout is
//! not a terminal (pipes, files, CI logs), and never when
//! `LYSOSOME_NO_HINTS` is set. The CLI keeps no state, so there is no
//! "shown already" bookkeeping; the line is simply easy to switch off.

use std::ffi::OsStr;

use crate::progress::ProgressMode;
use crate::ui::Painter;

/// Set to anything but empty or `0` to hide the hint.
pub const NO_HINTS_ENV: &str = "LYSOSOME_NO_HINTS";

/// The public page the hint links to (download links for macOS and Linux).
pub const APP_URL: &str = "https://lysosome.dev";

/// Whether this run may print the hint.
pub fn enabled(
    json: bool,
    progress: ProgressMode,
    stdout_tty: bool,
    no_hints: Option<&OsStr>,
) -> bool {
    let opted_out = no_hints.is_some_and(|v| !v.is_empty() && v != "0");
    !json && progress != ProgressMode::Json && stdout_tty && !opted_out
}

/// [`enabled`] for the current process.
pub fn enabled_now(json: bool, progress: ProgressMode) -> bool {
    enabled(
        json,
        progress,
        crate::ui::stdout_is_tty(),
        std::env::var_os(NO_HINTS_ENV).as_deref(),
    )
}

/// The hint, as its own dim line (with a blank line before it). It fits in
/// 80 columns; how to hide it is in `lysosome --help` and the README.
pub fn line(p: &Painter) -> String {
    format!(
        "\n  {}\n",
        p.dim(format!(
            "Tip: the free lysosome desktop app watches this for you: {APP_URL}"
        ))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn only_for_people_at_a_terminal() {
        let auto = ProgressMode::Auto;
        assert!(enabled(false, auto, true, None));
        assert!(enabled(false, ProgressMode::Off, true, None));
        // Machine-readable output never gets it.
        assert!(!enabled(true, auto, true, None));
        assert!(!enabled(false, ProgressMode::Json, true, None));
        // Piped or redirected stdout never gets it.
        assert!(!enabled(false, auto, false, None));
    }

    #[test]
    fn env_var_switches_it_off() {
        let auto = ProgressMode::Auto;
        let v = |s: &str| OsString::from(s);
        assert!(!enabled(false, auto, true, Some(&v("1"))));
        assert!(!enabled(false, auto, true, Some(&v("yes"))));
        // Empty or "0" means "not set".
        assert!(enabled(false, auto, true, Some(&v(""))));
        assert!(enabled(false, auto, true, Some(&v("0"))));
    }

    #[test]
    fn line_is_one_short_plain_line_without_color() {
        let l = line(&Painter::new(false));
        assert_eq!(l.trim().lines().count(), 1, "{l}");
        assert!(l.contains(APP_URL), "{l}");
        assert!(l.trim_matches('\n').chars().count() <= 80, "{l}");
        assert!(!l.contains('\u{1b}'), "{l}");
    }
}
