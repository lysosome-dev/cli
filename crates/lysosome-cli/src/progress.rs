//! An indicatif spinner on stderr that implements core's `Progress` hooks.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lysosome_core::progress::{Phase, Progress};

pub struct Spinner {
    pb: ProgressBar,
    walking: String,
}

impl Spinner {
    /// `walking` describes the roots, e.g. `~/code, ~/.worktrees`.
    pub fn new(walking: String) -> Self {
        let pb = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr());
        pb.set_style(style("  {spinner:.cyan} {msg}"));
        pb.set_message("Starting");
        pb.enable_steady_tick(Duration::from_millis(80));
        Spinner { pb, walking }
    }

    pub fn clear(&self) {
        self.pb.finish_and_clear();
    }
}

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
        .progress_chars("━╸─")
}

impl Progress for Spinner {
    fn start(&self, phase: Phase, total: Option<u64>) {
        let label = match phase {
            Phase::Walk => format!("Looking for worktrees in {}", self.walking),
            Phase::Git => "Reading git state".into(),
            Phase::Size => "Measuring disk usage".into(),
            Phase::Analyze => "Reading git state and sizes".into(),
            Phase::IgnoreCheck => "Checking ignored artifacts".into(),
            _ => phase.to_string(),
        };
        self.pb.set_position(0);
        match total {
            Some(n) => {
                self.pb.set_length(n);
                self.pb.set_style(style(
                    "  {spinner:.cyan} {msg:<28} {bar:24.cyan/blue.dim} {pos}/{len}",
                ));
            }
            None => {
                self.pb.unset_length();
                self.pb
                    .set_style(style("  {spinner:.cyan} {msg} {human_pos:.dim} folders"));
            }
        }
        self.pb.set_message(label);
    }

    fn advance(&self, n: u64) {
        self.pb.inc(n);
    }
}
