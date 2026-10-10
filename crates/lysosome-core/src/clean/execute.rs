//! Carry out a [`Plan`]: re-check, act, log. Items are handled one at a time,
//! in plan order; a failure is recorded and the next item still runs.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Delete `path` for good. Never follows symlinks (a symlink target is
/// refused; links inside a folder are unlinked, not followed) and never
/// touches another filesystem: the target must not be a mount point and
/// nothing may be mounted inside it. Folders are removed with std's
/// `remove_dir_all`, which walks with `openat`/`O_NOFOLLOW`, so a folder
/// swapped for a symlink mid-delete is not followed.
pub fn delete_permanently(path: &Path) -> Result<(), String> {
    let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if m.file_type().is_symlink() {
        return Err("refusing to delete a symlink target".into());
    }
    if m.is_dir() {
        recheck::inspect_tree(path, false)?;
        fs::remove_dir_all(path).map_err(|e| format!("could not delete: {e}"))
    } else {
        fs::remove_file(path).map_err(|e| format!("could not delete: {e}"))
    }
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
    /// An `attempt` entry is appended before acting on each item and its
    /// outcome after. If a write fails, that item and every remaining one are
    /// reported `failed` (the remaining ones are not attempted).
    pub log: Option<&'a ActionLog>,
    /// Set (e.g. by a Ctrl-C handler) to stop after the current item; the
    /// remaining items are reported `skipped: not attempted: interrupted`.
    pub stop: Option<&'a AtomicBool>,
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
            stop: None,
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
            let interrupted = self.stop.is_some_and(|s| s.load(Ordering::SeqCst));
            let started = match (&log_broken, self.log) {
                (None, Some(log)) if !interrupted => log.record_attempt(item).err(),
                _ => None,
            };
            if let Some(e) = started {
                log_broken = Some(e.to_string());
            }
            let r = match &log_broken {
                Some(e) => ItemResult::fail(
                    None,
                    format!("not attempted: the action log could not be written ({e})"),
                ),
                None if interrupted => ItemResult::skip("not attempted: interrupted"),
                None => {
                    let ctx = Ctx {
                        git: git.as_ref(),
                        home: &self.home,
                        dormant_after_days: self.dormant_after_days,
                        now: SystemTime::now(),
                        path_env: self.path_env.as_ref(),
                    };
                    let mut r = self.one(&ctx, item, plan.permanent);
                    if let Some(log) = self.log {
                        if let Err(e) = log.record(item, &r) {
                            // Whatever happened to this item is unrecorded:
                            // report it as a failure and stop acting.
                            r.message = format!(
                                "{} — but the action log could not be written ({e})",
                                if r.message.is_empty() {
                                    "done"
                                } else {
                                    &r.message
                                }
                            );
                            r.outcome = Outcome::Failed;
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

    fn one(&self, ctx: &Ctx<'_>, item: &PlanItem, permanent: bool) -> ItemResult {
        match self.try_one(ctx, item, permanent) {
            Ok(r) => r,
            Err(reason) => ItemResult::skip(reason),
        }
    }

    /// `Err` = skip with that reason.
    fn try_one(
        &self,
        ctx: &Ctx<'_>,
        item: &PlanItem,
        permanent: bool,
    ) -> Result<ItemResult, String> {
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
                self.remove_worktree(ctx, main_repo, path, item.size_bytes, permanent)
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
                recheck::no_repo_between(worktree, path)
                    .map_err(|e| format!("changed since scan: {e}"))?;
                recheck::still_ignored(ctx, worktree, path)
                    .map_err(|e| format!("changed since scan: {e}"))?;
                let size = self.checked_size(path)?;
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
                recheck::no_repo_between(project, path)
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
                let size = self.checked_size(path)?;
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
                        Err(e) if e == recheck::GONE => continue,
                        Err(e) => return Err(format!("changed since scan: {e}")),
                    }
                    let size = if fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
                        recheck::inspect_tree(p, false)
                            .map_err(|e| format!("changed since scan: {e}"))?
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
    fn checked_size(&self, path: &Path) -> Result<u64, String> {
        recheck::inspect_tree(path, true).map_err(|e| format!("changed since scan: {e}"))
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

    /// Ignored files that `git worktree remove` would delete for good and that
    /// aren't regenerable build output (`.env`, local databases, editor
    /// settings...). Artifact folders are left to git.
    fn precious_ignored(&self, ctx: &Ctx<'_>, path: &Path) -> Result<Vec<PathBuf>, String> {
        let git = ctx
            .git
            .ok_or("git could not be run, so it can't be re-checked")?;
        let out = git.run(
            path,
            [
                "status",
                "--porcelain=v1",
                "-z",
                "--ignored",
                "--untracked-files=normal",
            ],
        );
        if !out.success {
            return Err("could not list its ignored files".into());
        }
        let mut keep = Vec::new();
        for e in out.stdout.split(|b| *b == 0).filter(|e| e.len() > 3) {
            if !e.starts_with(b"!! ") {
                continue;
            }
            let rel = &e[3..];
            let rel = rel.strip_suffix(b"/").unwrap_or(rel);
            use std::os::unix::ffi::OsStrExt;
            let abs = path.join(std::ffi::OsStr::from_bytes(rel));
            let is_dir = fs::symlink_metadata(&abs).is_ok_and(|m| m.is_dir());
            let artifact = abs
                .file_name()
                .is_some_and(crate::artifacts::is_artifact_dir);
            if !(artifact && is_dir) && abs.starts_with(path) && abs != path {
                keep.push(abs);
            }
        }
        Ok(keep)
    }

    fn remove_worktree(
        &self,
        ctx: &Ctx<'_>,
        main_repo: &Path,
        path: &Path,
        size: u64,
        permanent: bool,
    ) -> Result<ItemResult, String> {
        let dest = Some("git worktree remove".to_string());
        // Pre-flight, before anything moves: git would delete everything in
        // the folder, so nothing in it may be a nested checkout (submodule,
        // clone inside an ignored folder), unreadable, or a mount.
        let total =
            recheck::inspect_worktree(path).map_err(|e| format!("kept the worktree: {e}"))?;
        // `git worktree remove` deletes ignored files outright. Move the ones
        // that aren't build output to the Trash first (unless --permanent).
        let mut rescued: Vec<PathBuf> = Vec::new();
        let mut rescued_bytes = 0u64;
        let rescued_note = |r: &[PathBuf]| -> String {
            if r.is_empty() {
                return String::new();
            }
            let names: Vec<String> = r
                .iter()
                .take(5)
                .map(|p| p.strip_prefix(path).unwrap_or(p).display().to_string())
                .collect();
            format!(
                "; {} ignored {} already moved to the Trash ({}{})",
                r.len(),
                if r.len() == 1 {
                    "entry was"
                } else {
                    "entries were"
                },
                names.join(", "),
                if r.len() > 5 { ", …" } else { "" }
            )
        };
        if !permanent {
            for p in self.precious_ignored(ctx, path)? {
                let size = match fs::symlink_metadata(&p) {
                    Ok(m) if m.is_dir() => recheck::inspect_tree(&p, false).unwrap_or(0),
                    Ok(_) => crate::size::allocated_size(&p),
                    Err(_) => 0,
                };
                match self.trasher.trash(&p) {
                    Ok(_) => {
                        rescued.push(p);
                        rescued_bytes += size;
                    }
                    Err(e) => {
                        let msg = format!(
                            "kept the worktree: could not move ignored {} to the Trash first: {e}{}",
                            p.display(),
                            rescued_note(&rescued)
                        );
                        return Ok(ItemResult::fail(dest, msg));
                    }
                }
            }
        }
        let env = self.git_env();
        let git = Path::new("git");
        let hardened = |rest: &[&str]| -> Vec<String> {
            let mut a: Vec<String> = vec![
                "--no-optional-locks".into(),
                "-c".into(),
                "core.fsmonitor=false".into(),
                "-c".into(),
                "core.hooksPath=/dev/null".into(),
                "-C".into(),
                main_repo.to_string_lossy().into_owned(),
            ];
            a.extend(rest.iter().map(|s| s.to_string()));
            a
        };
        let remove_args = {
            let mut a = hardened(&["worktree", "remove"]);
            a.push(path.to_string_lossy().into_owned());
            a
        };
        debug_assert_eq!(
            &remove_args[7..],
            &worktree_remove_argv(main_repo, path)[3..]
        );
        let out = process::run(&Proc {
            program: git,
            args: &remove_args,
            env: &env,
            env_remove: REPO_ENV_VARS,
            timeout: self.git_timeout,
        });
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                return Ok(ItemResult::fail(
                    dest,
                    format!("could not run git: {e}{}", rescued_note(&rescued)),
                ))
            }
        };
        if !out.success {
            let why = if out.timed_out {
                "timed out".to_string()
            } else {
                out.summary()
            };
            return Ok(ItemResult::fail(
                dest,
                format!(
                    "git worktree remove failed: {why}{}",
                    rescued_note(&rescued)
                ),
            ));
        }
        if fs::symlink_metadata(path).is_ok() {
            return Ok(ItemResult::fail(
                dest,
                "git reported success but the folder is still there",
            ));
        }
        let pruned = process::run(&Proc {
            program: git,
            args: &hardened(&["worktree", "prune"]),
            env: &env,
            env_remove: REPO_ENV_VARS,
            timeout: self.git_timeout,
        })
        .is_ok_and(|o| o.success);
        let mut msg = if pruned {
            "removed with `git worktree remove` and pruned; the branch is kept".to_string()
        } else {
            "removed with `git worktree remove` (prune failed); the branch is kept".to_string()
        };
        if !rescued.is_empty() {
            write!(
                msg,
                "{}",
                rescued_note(&rescued).replace("already moved", "moved")
            )
            .ok();
        }
        let _ = size; // the scan's estimate; report what was measured now
        Ok(ItemResult::done(
            total.saturating_sub(rescued_bytes),
            "git worktree remove",
            msg,
        ))
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
