//! The detector registry and the runner that merges detectors into one report.
//!
//! * [`project_detectors`] — scanners driven by the scan roots (worktrees,
//!   artifacts). They share one walk through [`ScanContext::projects`].
//! * [`all_detectors`] — everything `lysosome scan` runs: the project
//!   detectors plus the machine-wide [`crate::system`] detectors.
//! * [`run`] — runs detectors concurrently and builds the combined [`Report`].
//!
//! Adding a scanner: implement [`Detector`] and append it to [`all_detectors`].
//! The CLI renders every category generically (category display name, tier,
//! status, size, `last_activity`; rows group under `details.project` when
//! present), so a new detector needs no CLI changes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::detector::{ArtifactDetector, Detector, DetectorOutput, ScanContext, WorktreeDetector};
use crate::error::Error;
use crate::model::{Category, Report, Skipped};
use crate::projects::ProjectScan;

/// Detectors that scan the user's roots: worktrees, then project artifacts.
pub fn project_detectors() -> Vec<Box<dyn Detector>> {
    vec![Box::new(WorktreeDetector), Box::new(ArtifactDetector)]
}

/// Everything `lysosome scan` runs: project detectors, then the system
/// detectors (caches, AI models, Docker, Xcode, Android, editors).
pub fn all_detectors() -> Vec<Box<dyn Detector>> {
    let mut all = project_detectors();
    all.extend(crate::system::detectors());
    all
}

/// How one detector did.
#[derive(Debug)]
pub struct DetectorRun {
    pub id: &'static str,
    pub category: Category,
    pub elapsed: Duration,
    pub findings: usize,
    /// Set when the detector failed; its findings are then missing from the report.
    pub error: Option<Error>,
}

/// Output of [`run`].
#[derive(Debug)]
pub struct ScanRun {
    pub report: Report,
    /// One entry per detector, in registry order.
    pub runs: Vec<DetectorRun>,
    /// The shared project scan, when a project detector ran it (worktree
    /// details, stale records, phase timings).
    pub projects: Option<Arc<ProjectScan>>,
    pub total: Duration,
}

impl ScanRun {
    /// True when every detector failed (nothing useful to show).
    pub fn all_failed(&self) -> bool {
        !self.runs.is_empty() && self.runs.iter().all(|r| r.error.is_some())
    }
}

/// Run `detectors` concurrently (one thread each; detectors parallelize
/// internally) and merge their output. Findings keep registry order.
/// A failing detector is recorded in [`ScanRun::runs`], never fatal.
pub fn run(detectors: &[Box<dyn Detector>], ctx: &ScanContext<'_>) -> ScanRun {
    let started = Instant::now();
    let results: Vec<(Duration, crate::Result<DetectorOutput>)> = std::thread::scope(|s| {
        let handles: Vec<_> = detectors
            .iter()
            .map(|d| {
                s.spawn(move || {
                    let t = Instant::now();
                    let out = d.run(ctx);
                    (t.elapsed(), out)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });

    let projects = ctx.projects_if_scanned();
    let mut findings = Vec::new();
    let mut stale = Vec::new();
    let mut skipped = projects.as_ref().map(|p| p.skipped()).unwrap_or_default();
    let mut runs = Vec::with_capacity(detectors.len());
    for (d, (elapsed, out)) in detectors.iter().zip(results) {
        let mut run = DetectorRun {
            id: d.id(),
            category: d.category(),
            elapsed,
            findings: 0,
            error: None,
        };
        match out {
            Ok(o) => {
                run.findings = o.findings.len();
                findings.extend(o.findings);
                stale.extend(o.stale_metadata);
                merge_skipped(&mut skipped, o.skipped);
            }
            Err(e) => run.error = Some(e),
        }
        runs.push(run);
    }
    let roots = projects
        .as_ref()
        .map(|p| p.worktrees.roots.clone())
        .unwrap_or_else(|| ctx.roots.clone());
    ScanRun {
        report: Report::new(roots, ctx.dormant_after_days, findings, stale, skipped),
        runs,
        projects,
        total: started.elapsed(),
    }
}

fn merge_skipped(into: &mut Skipped, other: Skipped) {
    into.permission_denied += other.permission_denied;
    into.other_errors += other.other_errors;
    for r in other.missing_roots {
        if !into.missing_roots.contains(&r) {
            into.missing_roots.push(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Finding, Status, Tier};

    struct Fake(Category, Option<u64>);

    impl Detector for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn category(&self) -> Category {
            self.0
        }
        fn scan(&self, _ctx: &ScanContext<'_>) -> crate::Result<Vec<Finding>> {
            let Some(size) = self.1 else {
                return Err(Error::NoHomeDir);
            };
            Ok(vec![Finding {
                id: "x:/x".into(),
                category: self.0,
                kind: "x".into(),
                path: "/x".into(),
                size_bytes: size,
                tier: Tier::Regenerable,
                status: Status::Removable,
                reason: "r".into(),
                last_activity: None,
                details: serde_json::json!({}),
                cleanup: None,
            }])
        }
    }

    #[test]
    fn merges_detectors_and_records_failures() {
        let ctx = ScanContext::new(vec!["/nonexistent".into()], "/nonexistent".into());
        let dets: Vec<Box<dyn Detector>> = vec![
            Box::new(Fake(Category::Xcode, Some(7))),
            Box::new(Fake(Category::AiModels, None)),
        ];
        let out = run(&dets, &ctx);
        assert_eq!(out.report.findings.len(), 1);
        assert_eq!(out.report.summary.by_category[&Category::Xcode], 7);
        assert!(out.runs[1].error.is_some());
        assert!(!out.all_failed());
        assert!(out.projects.is_none());
    }

    #[test]
    fn registry_lists_project_then_system_detectors() {
        let ids: Vec<_> = all_detectors().iter().map(|d| d.id()).collect();
        assert_eq!(ids[..2], ["worktrees", "artifacts"]);
        for id in [
            "ollama",
            "huggingface",
            "docker",
            "npm",
            "homebrew",
            "android-avd",
            "vscode",
        ] {
            assert!(ids.contains(&id), "{id} missing from {ids:?}");
        }
        assert_eq!(ids.len(), 2 + crate::system::detectors().len());
    }
}
