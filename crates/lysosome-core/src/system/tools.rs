//! Running other developer tools (`brew --cache`, `go env`, `docker system df`,
//! `xcrun simctl`...) to ask them where their data lives.
//!
//! * Programs are looked up on the `PATH` of the [`SystemEnv`](super::SystemEnv),
//!   not the process, so tests can point it at fake scripts.
//! * Every call has a timeout; a missing tool, a non-zero exit or a timeout all
//!   come back as `None`. A slow or broken tool never fails a scan.
//! * stdin is closed and stderr discarded: tools can never prompt.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Default per-call timeout for "where is your cache?" questions.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// Default timeout for liveness probes (`docker version`): a daemon that is
/// up answers in milliseconds.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Spawns tool subprocesses with a fixed `PATH` and `HOME`.
#[derive(Debug, Clone)]
pub struct Tools {
    path: Option<OsString>,
    home: Option<PathBuf>,
    timeout: Duration,
}

impl Tools {
    pub fn new(path: Option<OsString>, home: Option<PathBuf>, timeout: Duration) -> Self {
        Tools {
            path,
            home,
            timeout,
        }
    }

    /// Absolute path of `program` on our `PATH`, if it is an executable file.
    pub fn which(&self, program: &str) -> Option<PathBuf> {
        let path = self.path.as_ref()?;
        std::env::split_paths(path)
            .filter(|d| d.is_absolute())
            .map(|d| d.join(program))
            .find(|p| is_executable(p))
    }

    /// Run `program args...` and return trimmed stdout on success.
    pub fn output<I, S>(&self, program: &str, args: I) -> Option<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_with_timeout(program, args, self.timeout)
    }

    /// Like [`Tools::output`] with an explicit timeout.
    pub fn output_with_timeout<I, S>(
        &self,
        program: &str,
        args: I,
        timeout: Duration,
    ) -> Option<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let exe = self.which(program)?;
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let mut child = None;
        // A script written by another thread just before we fork can briefly
        // report ETXTBSY on Linux; retry a few times.
        for attempt in 0..5 {
            let mut cmd = Command::new(&exe);
            // Own process group, so a timeout kills wrapper scripts *and* the
            // programs they started (pyenv shims, `brew`'s ruby, node...).
            cmd.process_group(0);
            cmd.args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            if let Some(p) = &self.path {
                cmd.env("PATH", p);
            }
            if let Some(h) = &self.home {
                cmd.env("HOME", h);
            }
            match cmd.spawn() {
                Ok(c) => {
                    child = Some(c);
                    break;
                }
                Err(e) if e.raw_os_error() == Some(26) && attempt < 4 => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return None,
            }
        }
        let mut child = child?;
        let (tx, rx) = mpsc::channel();
        if let Some(mut out) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = out.read_to_end(&mut buf);
                let _ = tx.send(buf);
            });
        }
        // Poll instead of `wait_timeout`: its SIGCHLD-based wait can miss an
        // exit when several threads wait on children at once.
        let deadline = Instant::now() + timeout;
        let mut pause = Duration::from_millis(2);
        let pid = child.id();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(50));
                }
                _ => {
                    kill_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        // A leftover grandchild may still hold stdout open; never wait past
        // the deadline for it.
        let left = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(100));
        let stdout = match rx.recv_timeout(left) {
            Ok(buf) => buf,
            Err(_) => {
                kill_group(pid);
                return None;
            }
        };
        if !status?.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&stdout).trim().to_string())
    }

    /// First line of a tool's output as an absolute path to an existing folder.
    pub fn dir_output<I, S>(&self, program: &str, args: I) -> Option<PathBuf>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.output(program, args)?;
        let p = PathBuf::from(out.lines().next()?.trim());
        (p.is_absolute() && p.is_dir()).then_some(p)
    }
}

/// SIGKILL the process group led by `pid` (see `process_group(0)` above).
fn kill_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain syscall; a negative pid addresses the group we created.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn timeout_kills_grandchildren_holding_stdout() {
        let t = tempfile::tempdir().unwrap();
        let bin = t.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("slowtool");
        // A wrapper whose child (not exec'd) keeps stdout open.
        std::fs::write(&script, "#!/bin/sh\n/bin/sleep 60\necho done\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tools = Tools::new(Some(bin.into_os_string()), None, Duration::from_millis(300));
        let start = Instant::now();
        let out = tools.output("slowtool", ["x"]);
        assert!(out.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
    }
}
