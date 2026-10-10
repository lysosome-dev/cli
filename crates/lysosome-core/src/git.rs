//! Read-only `git` subprocess wrapper.
//!
//! Every invocation:
//! * passes `--no-optional-locks` (and sets `GIT_OPTIONAL_LOCKS=0`) so git never
//!   refreshes the index or takes optional locks (safety rule 1);
//! * disables `core.fsmonitor` so no daemon is spawned on the user's behalf;
//! * never prompts (`GIT_TERMINAL_PROMPT=0`, stdin closed unless input is given);
//! * is killed after a timeout.
//!
//! Concurrency is bounded by running callers inside [`Git::pool`], a dedicated
//! rayon pool of `min(8, cores)` threads, so a scan of 300 worktrees never
//! forks 300 gits at once.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use wait_timeout::ChildExt;

use crate::error::{Error, Result};

/// Environment variables that point git at a specific repository; always cleared.
pub(crate) const REPO_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// Default per-call timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Result of one git call. A spawn failure or timeout is reported as an
/// unsuccessful output with empty stdout (same as the prototype).
#[derive(Debug, Clone, Default)]
pub struct GitOutput {
    pub success: bool,
    /// Exit code, `None` if the process was killed or never started.
    pub code: Option<i32>,
    /// Raw stdout bytes.
    pub stdout: Vec<u8>,
    pub timed_out: bool,
}

impl GitOutput {
    /// Stdout as (lossy) UTF-8, trimmed of surrounding whitespace.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// Runs git subprocesses.
#[derive(Debug)]
pub struct Git {
    timeout: Duration,
    /// Extra environment for every call (tests isolate HOME/GIT_CONFIG_GLOBAL).
    envs: Vec<(OsString, OsString)>,
    pool: rayon::ThreadPool,
    calls: AtomicU64,
}

impl Git {
    /// Verify `git` is runnable and build the bounded worker pool.
    pub fn new(timeout: Duration, concurrency: Option<usize>) -> Result<Self> {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .status()
            .map_err(Error::GitUnavailable)?;
        let threads = concurrency.unwrap_or_else(default_concurrency).max(1);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("lysosome-git-{i}"))
            .build()
            .map_err(|e| Error::ThreadPool(e.to_string()))?;
        Ok(Git {
            timeout,
            pool,
            calls: AtomicU64::new(0),
            envs: Vec::new(),
        })
    }

    /// Add an environment variable to every git call.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// The bounded pool git-heavy work should run in (`git.pool().install(..)`).
    pub fn pool(&self) -> &rayon::ThreadPool {
        &self.pool
    }

    /// Number of git processes spawned so far.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// `git --no-optional-locks -C <dir> <args...>`.
    pub fn run<I, S>(&self, dir: &Path, args: I) -> GitOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_input(dir, args, None)
    }

    /// Like [`Git::run`], feeding `input` on stdin.
    pub fn run_with_input<I, S>(&self, dir: &Path, args: I, input: Option<&[u8]>) -> GitOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut cmd = Command::new("git");
        // Inherited repo-redirecting variables (set inside git hooks, `rebase
        // --exec`, some IDEs) would make `-C <worktree>` analyze another repo.
        for var in REPO_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.arg("--no-optional-locks")
            .args(["-c", "core.fsmonitor=false"])
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .envs(self.envs.iter().map(|(k, v)| (k, v)))
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let Ok(mut child) = cmd.spawn() else {
            return GitOutput::default();
        };

        // Feed stdin and drain stdout on helper threads so a large output
        // (e.g. thousands of untracked files) can't deadlock the pipe.
        let writer = match (input, child.stdin.take()) {
            (Some(data), Some(mut stdin)) => {
                let data = data.to_vec();
                Some(std::thread::spawn(move || {
                    let _ = stdin.write_all(&data);
                }))
            }
            _ => None,
        };
        let reader = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = out.read_to_end(&mut buf);
                buf
            })
        });

        let (status, timed_out) = match child.wait_timeout(self.timeout) {
            Ok(Some(status)) => (Some(status), false),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                (None, true)
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                (None, false)
            }
        };
        if let Some(w) = writer {
            let _ = w.join();
        }
        let stdout = reader.and_then(|r| r.join().ok()).unwrap_or_default();
        match status {
            Some(s) => GitOutput {
                success: s.success(),
                code: s.code(),
                stdout,
                timed_out,
            },
            None => GitOutput {
                timed_out,
                ..GitOutput::default()
            },
        }
    }
}

/// `min(8, available cores)`.
pub fn default_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_git_and_reports_failure_cleanly() {
        let t = tempfile::tempdir().unwrap();
        let cfg = t.path().join("gitconfig");
        std::fs::write(&cfg, "").unwrap();
        let git = Git::new(DEFAULT_TIMEOUT, Some(2))
            .unwrap()
            .with_env("HOME", t.path())
            .with_env("GIT_CONFIG_GLOBAL", &cfg)
            .with_env("GIT_CONFIG_NOSYSTEM", "1");
        let out = git.run(t.path(), ["rev-parse", "--is-inside-work-tree"]);
        assert!(!out.success, "temp dir is not a repo");
        let out = git.run(t.path(), ["--version"]);
        assert!(out.success);
        assert!(out.text().starts_with("git version"));
        assert_eq!(git.calls(), 2);
    }
}
