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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// Default per-call timeout for "where is your cache?" questions.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(10);

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
        let reader = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = out.read_to_end(&mut buf);
                buf
            })
        });
        let status = match child.wait_timeout(timeout) {
            Ok(Some(s)) => Some(s),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        };
        let stdout = reader.and_then(|r| r.join().ok()).unwrap_or_default();
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

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
