//! Run a cleanup program (git, a tool's own cleanup command): argv only, no
//! shell, stdin closed, stdout and stderr captured, killed (with its whole
//! process group) after a timeout.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// What a finished (or killed) process produced.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

impl ProcOutput {
    /// One-line summary: the last non-empty line of stderr, else of stdout.
    pub fn summary(&self) -> String {
        let pick = |b: &[u8]| {
            String::from_utf8_lossy(b)
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string)
        };
        let line = pick(&self.stderr)
            .or_else(|| pick(&self.stdout))
            .unwrap_or_default();
        truncate(&line, 300)
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Spec for one run.
pub(crate) struct Proc<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    pub env: &'a [(OsString, OsString)],
    pub env_remove: &'a [&'a str],
    pub timeout: Duration,
}

/// Look `program` up on `path` (a `PATH`-style list). Absolute programs are
/// used as is if executable.
pub(crate) fn which(program: &str, path: Option<&OsString>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let exe = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let p = PathBuf::from(program);
        return (p.is_absolute() && exe(&p)).then_some(p);
    }
    std::env::split_paths(path?)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(program))
        .find(|p| exe(p))
}

pub(crate) fn run(spec: &Proc<'_>) -> std::io::Result<ProcOutput> {
    let mut cmd = Command::new(spec.program);
    cmd.process_group(0)
        .args(spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for k in spec.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in spec.env {
        cmd.env(k, v);
    }
    let mut child = None;
    let mut last_err = None;
    // A script written just before we fork can briefly report ETXTBSY on Linux.
    for _ in 0..5 {
        match cmd.spawn() {
            Ok(c) => {
                child = Some(c);
                break;
            }
            Err(e) if e.raw_os_error() == Some(26) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
    let Some(mut child) = child else {
        return Err(last_err.unwrap_or_else(|| std::io::Error::other("could not start")));
    };
    let pid = child.id();
    let drain = |r: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        if let Some(mut r) = r {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = r.read_to_end(&mut buf);
                let _ = tx.send(buf);
            });
        }
        rx
    };
    let out_rx = drain(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err_rx = drain(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + spec.timeout;
    let mut pause = Duration::from_millis(5);
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(s)) => break (Some(s), false),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(100));
            }
            Ok(None) => {
                kill_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Err(e) => {
                kill_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
    };
    // A leftover grandchild may hold the pipes open; don't wait forever.
    let grace = Duration::from_secs(2);
    let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
    let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
    Ok(ProcOutput {
        success: status.is_some_and(|s| s.success()),
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
    })
}

fn kill_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: plain syscall; the group id is our own child's pid.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_times_out() {
        let sh = PathBuf::from("/bin/sh");
        let args = vec!["-c".to_string(), "echo out; echo oops >&2; exit 3".into()];
        let out = run(&Proc {
            program: &sh,
            args: &args,
            env: &[],
            env_remove: &[],
            timeout: Duration::from_secs(20),
        })
        .unwrap();
        assert!(!out.success);
        assert_eq!(out.code, Some(3));
        assert_eq!(out.summary(), "oops");

        let args = vec!["-c".to_string(), "sleep 30".into()];
        let t = Instant::now();
        let out = run(&Proc {
            program: &sh,
            args: &args,
            env: &[],
            env_remove: &[],
            timeout: Duration::from_millis(300),
        })
        .unwrap();
        assert!(out.timed_out && !out.success);
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn which_needs_an_executable() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("tool");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        let path = Some(t.path().as_os_str().to_owned());
        assert_eq!(which("tool", path.as_ref()), None);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(which("tool", path.as_ref()), Some(p));
        assert_eq!(which("tool", None), None);
    }
}
