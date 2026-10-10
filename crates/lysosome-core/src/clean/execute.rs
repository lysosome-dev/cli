//! Carry out a [`Plan`]: re-check, act, log. Items are handled one at a time,
//! in plan order; a failure is recorded and the next item still runs.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use super::log::ActionLog;
use super::process::{self, Proc};
use super::recheck::{self, Ctx};
use super::{worktree_remove_argv, ActionKind, Guard, Plan, PlanItem};
use crate::git::{Git, REPO_ENV_VARS};
use crate::model::Status;
use crate::worktree::discover;

/// Default limit for one cleanup command (`docker builder prune` can be slow).
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Moves things somewhere they can be recovered from.
pub trait Trasher: Send + Sync {
    /// Move `path` (a file or folder, never followed if a symlink) out of the
    /// way, recoverably. Returns where it went, for the log.
    fn trash(&self, path: &Path) -> Result<String, String>;
}

/// The system Trash via the `trash` crate: the macOS Trash (via
/// `NSFileManager`, no Finder automation prompt) or the freedesktop.org trash
/// on Linux.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemTrash;

impl Trasher for SystemTrash {
    fn trash(&self, path: &Path) -> Result<String, String> {
        #[allow(unused_mut)]
        let mut ctx = trash::TrashContext::default();
        #[cfg(target_os = "macos")]
        {
            use trash::macos::{DeleteMethod, TrashContextExtMacos};
            ctx.set_delete_method(DeleteMethod::NsFileManager);
        }
        ctx.delete(path)
            .map_err(|e| format!("could not move to the Trash: {e}"))?;
        Ok("Trash".into())
    }
}

/// "Trash" into a folder of your choice (a rename, so it only works within
/// one filesystem). Used by tests and by `LYSOSOME_TRASH_DIR`.
#[derive(Debug, Clone)]
pub struct DirTrash {
    dir: PathBuf,
}

impl DirTrash {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        DirTrash { dir: dir.into() }
    }
}

impl Trasher for DirTrash {
    fn trash(&self, path: &Path) -> Result<String, String> {
        fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "item".into());
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut dest = self.dir.join(&name);
        let mut n = 0;
        while fs::symlink_metadata(&dest).is_ok() {
            n += 1;
            dest = self.dir.join(format!("{name}.{stamp}.{n}"));
        }
        fs::rename(path, &dest)
            .map_err(|e| format!("could not move to {}: {e}", dest.display()))?;
        Ok(dest.to_string_lossy().into_owned())
    }
}

/// Delete `path` for good. Symlinks are never followed: a symlink target is
/// refused, and `remove_dir_all` removes links inside the folder, not what
/// they point at.
pub fn delete_permanently(path: &Path) -> Result<(), String> {
    let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if m.file_type().is_symlink() {
        return Err("refusing to delete a symlink target".into());
    }
    let r = if m.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    r.map_err(|e| format!("could not delete: {e}"))
}

/// What happened to one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Removed (or the command succeeded).
    Done,
    /// Not touched: something changed since the scan, or it's already gone.
    Skipped,
    /// Tried and failed (the command failed, the Trash refused...).
    Failed,
}

/// Result of acting on one [`PlanItem`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemResult {
    pub outcome: Outcome,
    /// Bytes freed (measured right before acting where possible).
    pub freed_bytes: u64,
    /// Where it went: `Trash`, a trash folder path, `deleted permanently`,
    /// `git worktree remove`, `command`.
    pub destination: Option<String>,
    /// Why it was skipped, why it failed, or a short summary of what happened.
    pub message: String,
}

impl ItemResult {
    fn skip(reason: impl Into<String>) -> Self {
        ItemResult {
            outcome: Outcome::Skipped,
            freed_bytes: 0,
            destination: None,
            message: reason.into(),
        }
    }

    fn fail(dest: Option<String>, msg: impl Into<String>) -> Self {
        ItemResult {
            outcome: Outcome::Failed,
            freed_bytes: 0,
            destination: dest,
            message: msg.into(),
        }
    }

    fn done(freed: u64, dest: impl Into<String>, msg: impl Into<String>) -> Self {
        ItemResult {
            outcome: Outcome::Done,
            freed_bytes: freed,
            destination: Some(dest.into()),
            message: msg.into(),
        }
    }
}

/// Executes plans.
pub struct Executor<'a> {
    pub trasher: &'a dyn Trasher,
    /// The user's home folder: never a target, nor anything above it.
    pub home: PathBuf,
    /// Idle days for the worktree / project re-checks (same as the scan).
    pub dormant_after_days: u64,
    pub command_timeout: Duration,
    pub git_timeout: Duration,
    /// `PATH` used to find cleanup commands.
    pub path_env: Option<OsString>,
    /// Extra environment for git processes (tests isolate config).
    pub git_env: Vec<(OsString, OsString)>,
    /// Every outcome is appended here. If writing fails, the remaining items
    /// are skipped.
    pub log: Option<&'a ActionLog>,
}

impl<'a> Executor<'a> {
    pub fn new(trasher: &'a dyn Trasher, home: PathBuf, log: Option<&'a ActionLog>) -> Self {
        Executor {
            trasher,
            home,
            dormant_after_days: crate::worktree::DEFAULT_DORMANT_DAYS,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            git_timeout: Duration::from_secs(10 * 60),
            path_env: std::env::var_os("PATH"),
            git_env: Vec::new(),
            log,
        }
    }

    /// Re-check and act on every item of `plan`, in order. `on_item` is called
    /// after each one (for progress). Returns one result per item.
    pub fn execute(
        &self,
        plan: &Plan,
        on_item: &mut dyn FnMut(usize, &PlanItem, &ItemResult),
    ) -> crate::Result<Vec<ItemResult>> {
        // Cleaning caches doesn't need git; git-backed items are skipped
        // (never acted on unchecked) if it can't be run.
        let git = Git::new(crate::git::DEFAULT_TIMEOUT, Some(2))
            .ok()
            .map(|mut g| {
                for (k, v) in &self.git_env {
                    g = g.with_env(k.clone(), v.clone());
                }
                g
            });
        let mut results = Vec::with_capacity(plan.items.len());
        let mut log_broken: Option<String> = None;
        for (i, item) in plan.items.iter().enumerate() {
            let r = match &log_broken {
                Some(e) => ItemResult::skip(format!("not attempted: the action log failed ({e})")),
                None => {
                    let ctx = Ctx {
                        git: git.as_ref(),
                        home: &self.home,
                        dormant_after_days: self.dormant_after_days,
                        now: SystemTime::now(),
                        path_env: self.path_env.as_ref(),
                    };
                    let r = self.one(&ctx, item);
                    if let Some(log) = self.log {
                        if let Err(e) = log.record(item, &r) {
                            log_broken = Some(e.to_string());
                        }
                    }
                    r
                }
            };
            on_item(i, item, &r);
            results.push(r);
        }
        Ok(results)
    }

    fn one(&self, ctx: &Ctx<'_>, item: &PlanItem) -> ItemResult {
        match self.try_one(ctx, item) {
            Ok(r) => r,
            Err(reason) => ItemResult::skip(reason),
        }
    }

    /// `Err` = skip with that reason.
    fn try_one(&self, ctx: &Ctx<'_>, item: &PlanItem) -> Result<ItemResult, String> {
        let path = &item.path;
        match &item.guard {
            Guard::Worktree { main_repo, gitdir } => {
                if item.action != ActionKind::WorktreeRemove {
                    return Err("internal: unexpected action for a worktree".into());
                }
                recheck::protected(path, ctx.home)?;
                let (status, reason, main) = recheck::worktree(ctx, path, gitdir)
                    .map_err(|e| format!("changed since scan: {e}"))?;
                if status != Status::Removable {
                    return Err(changed(status, &reason));
                }
                if !main
                    .as_deref()
                    .is_some_and(|m| discover::same_path(m, main_repo))
                {
                    return Err("changed since scan: it belongs to a different repository".into());
                }
                Ok(self.remove_worktree(ctx, main_repo, path, item.size_bytes))
            }
            Guard::WorktreeArtifact { worktree, gitdir } => {
                recheck::protected(path, ctx.home)?;
                if !path.starts_with(worktree) || path == worktree {
                    return Err("not inside its worktree".into());
                }
                let (status, reason, _) = recheck::worktree(ctx, worktree, gitdir)
                    .map_err(|e| format!("changed since scan: {e}"))?;
                if !matches!(status, Status::Removable | Status::Review) {
                    return Err(changed(status, &reason));
                }
                recheck::real_target(path, true, None)
                    .map_err(|e| format!("changed since scan: {e}"))?;
                recheck::still_ignored(ctx, worktree, path)
                    .map_err(|e| format!("changed since scan: {e}"))?;
                let size = self.checked_size(path, item.size_bytes)?;
                Ok(self.dispose(item.action, &[(path.clone(), size)]))
            }
            Guard::RepoArtifact { project } => {
                recheck::protected(path, ctx.home)?;
                if !path.starts_with(project) || path == project {
                    return Err("not inside its project".into());
                }
                if !fs::symlink_metadata(project.join(".git")).is_ok_and(|m| m.is_dir()) {
                    return Err(
                        "changed since scan: the project is no longer a git repository".into(),
                    );
                }
                recheck::real_target(path, true, None)
                    .map_err(|e| format!("changed since scan: {e}"))?;
                recheck::still_ignored(ctx, project, path)
                    .map_err(|e| format!("changed since scan: {e}"))?;
                let (status, reason) = recheck::repo_status(ctx, project)?;
                if status != Status::Removable {
                    return Err(match status {
                        Status::Active => {
                            "changed since scan: the project was used recently".into()
                        }
                        s => changed(s, &reason),
                    });
                }
                let size = self.checked_size(path, item.size_bytes)?;
                Ok(self.dispose(item.action, &[(path.clone(), size)]))
            }
            Guard::Paths { parents } => {
                let mut todo = Vec::new();
                for (p, parent) in item.paths.iter().zip(parents) {
                    recheck::protected(p, ctx.home)?;
                    let Some(parent) = parent else {
                        return Err("could not verify its folder when planning".into());
                    };
                    match recheck::real_target(p, false, Some(parent)) {
                        Ok(()) => {}
                        Err(e) if e == "no longer exists" => continue,
                        Err(e) => return Err(format!("changed since scan: {e}")),
                    }
                    let size = if fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
                        recheck::measure_dir(p).map_or(0, |(s, _)| s)
                    } else {
                        crate::size::allocated_size(p)
                    };
                    todo.push((p.clone(), size));
                }
                if todo.is_empty() {
                    return Err("already gone".into());
                }
                Ok(self.dispose(item.action, &todo))
            }
            Guard::Command { existing_path } => {
                if item.action != ActionKind::Command || item.command.is_empty() {
                    return Err("internal: unexpected action for a command".into());
                }
                if let Some(p) = existing_path {
                    if fs::symlink_metadata(p).is_err() {
                        return Err("already gone".into());
                    }
                }
                let program = recheck::program(ctx, &item.command[0])?;
                Ok(self.run_command(
                    &program,
                    &item.command,
                    existing_path.as_deref(),
                    item.size_bytes,
                ))
            }
        }
    }

    /// Size right before acting; refuses folders that now hold a `.git`.
    fn checked_size(&self, path: &Path, planned: u64) -> Result<u64, String> {
        match recheck::measure_dir(path) {
            Some((_, true)) => {
                Err("changed since scan: it now contains a git checkout (.git inside)".into())
            }
            Some((size, false)) => Ok(size),
            None => Ok(planned),
        }
    }

    fn dispose(&self, action: ActionKind, targets: &[(PathBuf, u64)]) -> ItemResult {
        let mut freed = 0;
        let mut dests = Vec::new();
        let mut errors = Vec::new();
        for (p, size) in targets {
            let r = match action {
                ActionKind::Trash => self.trasher.trash(p),
                ActionKind::Delete => delete_permanently(p).map(|()| "deleted permanently".into()),
                _ => Err("internal: not a trash or delete action".into()),
            };
            match r {
                Ok(d) => {
                    freed += size;
                    if !dests.contains(&d) {
                        dests.push(d);
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", p.display())),
            }
        }
        let dest = (!dests.is_empty()).then(|| dests.join(", "));
        if errors.is_empty() {
            let msg = match action {
                ActionKind::Delete => "deleted permanently".to_string(),
                _ => "moved to the Trash".to_string(),
            };
            ItemResult::done(freed, dest.unwrap_or_default(), msg)
        } else {
            let mut r = ItemResult::fail(dest, errors.join("; "));
            r.freed_bytes = freed;
            r
        }
    }

    fn git_env(&self) -> Vec<(OsString, OsString)> {
        let mut env = vec![("GIT_TERMINAL_PROMPT".into(), "0".into())];
        env.extend(self.git_env.iter().cloned());
        env
    }

    fn remove_worktree(
        &self,
        _ctx: &Ctx<'_>,
        main_repo: &Path,
        path: &Path,
        size: u64,
    ) -> ItemResult {
        let argv = worktree_remove_argv(main_repo, path);
        let env = self.git_env();
        let git = Path::new("git");
        let out = process::run(&Proc {
            program: git,
            args: &argv[1..],
            env: &env,
            env_remove: REPO_ENV_VARS,
            timeout: self.git_timeout,
        });
        let dest = Some("git worktree remove".to_string());
        let out = match out {
            Ok(o) => o,
            Err(e) => return ItemResult::fail(dest, format!("could not run git: {e}")),
        };
        if !out.success {
            let why = if out.timed_out {
                "timed out".to_string()
            } else {
                out.summary()
            };
            return ItemResult::fail(dest, format!("git worktree remove failed: {why}"));
        }
        if fs::symlink_metadata(path).is_ok() {
            return ItemResult::fail(dest, "git reported success but the folder is still there");
        }
        let prune_args: Vec<String> = vec![
            "-C".into(),
            main_repo.to_string_lossy().into_owned(),
            "worktree".into(),
            "prune".into(),
        ];
        let pruned = process::run(&Proc {
            program: git,
            args: &prune_args,
            env: &env,
            env_remove: REPO_ENV_VARS,
            timeout: self.git_timeout,
        })
        .is_ok_and(|o| o.success);
        let msg = if pruned {
            "removed with `git worktree remove` and pruned; the branch is kept"
        } else {
            "removed with `git worktree remove` (prune failed); the branch is kept"
        };
        ItemResult::done(size, "git worktree remove", msg)
    }

    fn run_command(
        &self,
        program: &Path,
        argv: &[String],
        path: Option<&Path>,
        planned: u64,
    ) -> ItemResult {
        let dest = Some("command".to_string());
        let out = match process::run(&Proc {
            program,
            args: &argv[1..],
            env: &[],
            env_remove: &[],
            timeout: self.command_timeout,
        }) {
            Ok(o) => o,
            Err(e) => return ItemResult::fail(dest, format!("could not run `{}`: {e}", argv[0])),
        };
        if out.timed_out {
            return ItemResult::fail(
                dest,
                format!("timed out after {}s", self.command_timeout.as_secs()),
            );
        }
        if !out.success {
            let code = out
                .code
                .map_or("killed".to_string(), |c| format!("exit {c}"));
            let s = out.summary();
            return ItemResult::fail(
                dest,
                if s.is_empty() {
                    code
                } else {
                    format!("{code}: {s}")
                },
            );
        }
        // Measure what's left when the finding is a real folder; otherwise
        // trust the scan's estimate.
        let freed = match path {
            Some(p) => {
                let left = match fs::symlink_metadata(p) {
                    Ok(m) if m.is_dir() => recheck::measure_dir(p).map_or(0, |(s, _)| s),
                    Ok(_) => crate::size::allocated_size(p),
                    Err(_) => 0,
                };
                planned.saturating_sub(left)
            }
            None => planned,
        };
        let s = out.summary();
        ItemResult::done(
            freed,
            "command",
            if s.is_empty() { "ok".to_string() } else { s },
        )
    }
}

fn changed(status: Status, reason: &str) -> String {
    if reason.is_empty() {
        format!("changed since scan: now {}", status.as_str())
    } else {
        format!("changed since scan: now {} ({reason})", status.as_str())
    }
}
