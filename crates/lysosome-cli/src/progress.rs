//! Progress on stderr: an indicatif spinner for people (`--progress auto` on a
//! terminal) or newline-delimited JSON events for programs (`--progress json`,
//! schema/progress.schema.json). Both implement core's `Progress` hooks.

use std::io::Write as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use lysosome_core::model::Category;
use lysosome_core::progress::{Phase, Progress};
use serde_json::{json, Value};

use crate::ui;

/// `--progress`: how to report progress on stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum ProgressMode {
    /// A spinner when stderr is a terminal, nothing otherwise
    #[default]
    Auto,
    /// Newline-delimited JSON events (schema/progress.schema.json)
    Json,
    /// No progress output
    Off,
}

/// Human-readable label for a phase. `walking` describes the roots.
fn phase_label(phase: Phase, walking: &str) -> String {
    match phase {
        Phase::Walk if walking.is_empty() => "Looking for repositories".into(),
        Phase::Walk => format!("Looking for repositories in {walking}"),
        Phase::Git => "Reading git state".into(),
        Phase::Size => "Measuring disk usage".into(),
        Phase::Analyze => "Reading git state and sizes".into(),
        Phase::IgnoreCheck => "Checking ignored artifacts".into(),
        Phase::Artifacts => "Measuring project artifacts".into(),
        Phase::Github => "Asking GitHub about squash merges".into(),
        _ => phase.to_string(),
    }
}

/// Whatever `--progress` asked for. `None` from [`Reporter::new`] means quiet.
pub enum Reporter {
    Spinner(Spinner),
    Json(JsonProgress),
}

impl Reporter {
    /// The reporter for `mode`; `walking` describes the roots, e.g. `~/code, ~/.worktrees`.
    pub fn new(mode: ProgressMode, walking: String) -> Option<Self> {
        match mode {
            ProgressMode::Off => None,
            ProgressMode::Json => Some(Reporter::Json(JsonProgress::new(walking))),
            ProgressMode::Auto => {
                ui::stderr_is_tty().then(|| Reporter::Spinner(Spinner::new(walking)))
            }
        }
    }

    /// Replace the current message (for work without phases).
    pub fn set_message(&self, msg: impl Into<String>) {
        match self {
            Reporter::Spinner(s) => s.set_message(msg),
            Reporter::Json(j) => j.message(&msg.into()),
        }
    }

    /// Done: clear the spinner (JSON events need no cleanup).
    pub fn clear(&self) {
        if let Reporter::Spinner(s) = self {
            s.clear();
        }
    }

    /// A `warning` event in JSON mode; otherwise a `warning:` line on stderr.
    pub fn warn(reporter: Option<&Reporter>, p: &ui::Painter, msg: &str) {
        match reporter.and_then(Reporter::json) {
            Some(j) => j.warning(msg),
            None => eprintln!(
                "{} {msg}",
                p.paint("warning:", owo_colors::Style::new().yellow().bold())
            ),
        }
    }

    /// The JSON reporter, when that is what was asked for.
    pub fn json(&self) -> Option<&JsonProgress> {
        match self {
            Reporter::Json(j) => Some(j),
            Reporter::Spinner(_) => None,
        }
    }
}

impl Progress for Reporter {
    fn start(&self, phase: Phase, total: Option<u64>) {
        match self {
            Reporter::Spinner(s) => s.start(phase, total),
            Reporter::Json(j) => j.start(phase, total),
        }
    }

    fn advance(&self, n: u64) {
        match self {
            Reporter::Spinner(s) => s.advance(n),
            Reporter::Json(j) => j.advance(n),
        }
    }

    fn finish(&self, phase: Phase) {
        match self {
            Reporter::Spinner(s) => s.finish(phase),
            Reporter::Json(j) => j.finish(phase),
        }
    }

    fn detector_finished(
        &self,
        id: &'static str,
        category: Category,
        findings: usize,
        error: Option<&str>,
    ) {
        if let Reporter::Json(j) = self {
            j.detector_finished(id, category, findings, error);
        }
    }

    fn detector_findings(
        &self,
        id: &'static str,
        category: Category,
        findings: &[lysosome_core::model::Finding],
    ) {
        if let Reporter::Json(j) = self {
            j.detector_findings(id, category, findings);
        }
    }
}

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

    /// Replace the spinner's message (for scans without phases).
    pub fn set_message(&self, msg: impl Into<String>) {
        self.pb.set_message(msg.into());
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
        let label = phase_label(phase, &self.walking);
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

/// At most one `advance` event per phase this often.
const ADVANCE_EVERY: Duration = Duration::from_millis(100);

struct PhaseState {
    phase: Phase,
    total: Option<u64>,
    done: u64,
    /// `done` as of the last `advance` event.
    reported: u64,
    last: Instant,
}

/// Newline-delimited JSON progress events on stderr. Every line is one
/// object with `"type": "progress"` and an `event` field; see
/// schema/progress.schema.json. `advance` events are throttled.
pub struct JsonProgress {
    walking: String,
    started: Instant,
    /// Running phases, oldest first.
    phases: Mutex<Vec<PhaseState>>,
}

impl JsonProgress {
    pub fn new(walking: String) -> Self {
        JsonProgress {
            walking,
            started: Instant::now(),
            phases: Mutex::new(Vec::new()),
        }
    }

    fn emit(&self, event: &str, mut fields: Value) {
        fields["type"] = json!("progress");
        fields["event"] = json!(event);
        fields["elapsed_ms"] = json!(self.started.elapsed().as_millis() as u64);
        // One write per line so events from different threads never interleave.
        let mut line = fields.to_string();
        line.push('\n');
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }

    /// A free-form status message (work without phases).
    pub fn message(&self, msg: &str) {
        self.emit("message", json!({ "message": msg }));
    }

    /// Something the user should know (a root that isn't a folder, a
    /// detector that failed); the same text a terminal gets as `warning:`.
    pub fn warning(&self, msg: &str) {
        self.emit("warning", json!({ "message": msg }));
    }

    /// `clean`: the plan that is about to run.
    pub fn plan(&self, items: usize, bytes: u64) {
        self.emit("plan", json!({ "items": items, "bytes": bytes }));
    }

    /// `clean`: item `index` (0-based) of `total` was acted on.
    pub fn item(&self, index: usize, total: usize, id: &str, result: Value) {
        self.emit(
            "item",
            json!({ "index": index, "total": total, "id": id, "result": result }),
        );
    }

    fn start(&self, phase: Phase, total: Option<u64>) {
        {
            let mut phases = self.phases.lock().unwrap_or_else(|e| e.into_inner());
            phases.retain(|p| p.phase != phase);
            phases.push(PhaseState {
                phase,
                total,
                done: 0,
                reported: 0,
                last: Instant::now(),
            });
        }
        self.emit(
            "phase_start",
            json!({
                "phase": phase.as_str(),
                "label": phase_label(phase, &self.walking),
                "total": total,
            }),
        );
    }

    fn advance(&self, n: u64) {
        // Core reports advances without naming the phase; they belong to
        // the most recently started one that is still running.
        let due = {
            let mut phases = self.phases.lock().unwrap_or_else(|e| e.into_inner());
            let Some(p) = phases.last_mut() else { return };
            p.done += n;
            (p.last.elapsed() >= ADVANCE_EVERY).then(|| {
                p.last = Instant::now();
                p.reported = p.done;
                (p.phase, p.done, p.total)
            })
        };
        if let Some((phase, done, total)) = due {
            self.emit(
                "advance",
                json!({ "phase": phase.as_str(), "done": done, "total": total }),
            );
        }
    }

    fn finish(&self, phase: Phase) {
        let state = {
            let mut phases = self.phases.lock().unwrap_or_else(|e| e.into_inner());
            let i = phases.iter().position(|p| p.phase == phase);
            i.map(|i| phases.remove(i))
        };
        // The final count, if the last throttled event didn't carry it.
        if let Some(p) = state.filter(|p| p.done != p.reported) {
            self.emit(
                "advance",
                json!({ "phase": phase.as_str(), "done": p.done, "total": p.total }),
            );
        }
        self.emit("phase_finish", json!({ "phase": phase.as_str() }));
    }

    fn detector_finished(
        &self,
        id: &'static str,
        category: Category,
        findings: usize,
        error: Option<&str>,
    ) {
        self.emit(
            "detector",
            json!({
                "id": id,
                "category": category,
                "findings": findings,
                "error": error,
            }),
        );
    }

    fn detector_findings(
        &self,
        id: &'static str,
        category: Category,
        findings: &[lysosome_core::model::Finding],
    ) {
        self.emit(
            "findings",
            json!({ "id": id, "category": category, "findings": findings }),
        );
    }
}
