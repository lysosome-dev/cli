//! Running other developer tools (`brew --cache`, `go env`, `docker system df`,
//! `xcrun simctl`...) to ask them where their data lives.
//!
//! * Programs are looked up on the `PATH` of the [`SystemEnv`](super::SystemEnv),
//!   not the process, so tests can point it at fake scripts.
//! * Every call has a timeout; a missing tool, a non-zero exit or a timeout all
//!   come back as `None`. A slow or broken tool never fails a scan.
//! * stdin is closed and stderr discarded: tools can never prompt.
//! * Scanning never modifies anything (docs/SPEC.md §2.1), and that includes
//!   what the tools we ask do on our behalf. Every call:
//!   - starts from a cleared environment holding only the
//!     [`SystemEnv`](super::SystemEnv) variables, then [`SCAN_ENV`] on top, which
//!     turns off downloads, self-updates, auto-installs and bytecode writes;
//!   - runs in `/`, so a project's `packageManager`, `go.mod` toolchain line or
//!     `.tool-versions` in the current folder can't make a tool fetch itself;
//!   - refuses shims that download the real tool on first use (corepack's
//!     `pnpm` / `yarn` shims fill `~/.cache/node/corepack`; Volta's shims),
//!     so they are never run during a scan (callers fall back to well-known
//!     paths).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Default per-call timeout for "where is your cache?" questions.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// Default timeout for liveness probes (`docker version`): a daemon that is
/// up answers in milliseconds.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Variables set on every scan-time tool call, overriding the environment, so
/// asking a tool where its cache lives can never make it download, update,
/// install or write anything.
pub const SCAN_ENV: &[(&str, &str)] = &[
    // corepack (pnpm / yarn shims): no downloads, prompts or package.json pins.
    ("COREPACK_ENABLE_NETWORK", "0"),
    ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
    ("COREPACK_ENABLE_AUTO_PIN", "0"),
    ("COREPACK_ENABLE_STRICT", "0"),
    // npm / pnpm: no update check, no self-download of a pinned pnpm version.
    ("npm_config_update_notifier", "false"),
    ("npm_config_manage_package_manager_versions", "false"),
    ("pnpm_config_manage_package_manager_versions", "false"),
    // Homebrew: no auto-update or analytics. (Not HOMEBREW_NO_INSTALL_FROM_API:
    // without a local homebrew/core tap it makes brew clone one.)
    ("HOMEBREW_NO_AUTO_UPDATE", "1"),
    ("HOMEBREW_NO_ANALYTICS", "1"),
    ("HOMEBREW_NO_ENV_HINTS", "1"),
    // pip / Python: no prompt, no version-check state file, no .pyc files.
    ("PIP_NO_INPUT", "1"),
    ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
    ("PYTHONDONTWRITEBYTECODE", "1"),
    // uv: never touch the network.
    ("UV_OFFLINE", "1"),
    // Go: never download a newer toolchain.
    ("GOTOOLCHAIN", "local"),
    // mise shims (`mise exec`): never auto-install a missing tool version.
    ("MISE_EXEC_AUTO_INSTALL", "false"),
    ("MISE_NOT_FOUND_AUTO_INSTALL", "false"),
    // Docker CLI: no hints.
    ("DOCKER_CLI_HINTS", "false"),
];

/// Spawns tool subprocesses with a fixed environment, `PATH` and `HOME`.
#[derive(Debug, Clone)]
pub struct Tools {
    path: Option<OsString>,
    home: Option<PathBuf>,
    env: Arc<HashMap<String, OsString>>,
    timeout: Duration,
}

impl Tools {
    /// A runner whose children see only `PATH`, `HOME` and [`SCAN_ENV`]; add
    /// more variables with [`Tools::with_env`].
    pub fn new(path: Option<OsString>, home: Option<PathBuf>, timeout: Duration) -> Self {
        Tools {
            path,
            home,
            env: Arc::default(),
            timeout,
        }
    }

    /// Base environment for children (`PATH`, `HOME` and [`SCAN_ENV`] still win;
    /// `PWD` / `OLDPWD` are dropped since children run in `/`).
    pub fn with_env(mut self, vars: Arc<HashMap<String, OsString>>) -> Self {
        self.env = vars;
        self
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
        if is_downloading_shim(&exe) {
            return None;
        }
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
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .env_clear()
                .envs(
                    self.env
                        .iter()
                        .filter(|(k, _)| *k != "PWD" && *k != "OLDPWD"),
                );
            if let Some(p) = &self.path {
                cmd.env("PATH", p);
            }
            if let Some(h) = &self.home {
                cmd.env("HOME", h);
            }
            cmd.envs(SCAN_ENV.iter().copied());
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

/// Whether `exe` is a shim that may download the real tool when run, so a
/// scan never runs it: one of corepack's package-manager shims (a link into
/// corepack, or a script that hands off to it), or Volta's `volta-shim`.
/// Errs towards `true`: a false positive only means we use the well-known
/// default location instead of asking.
pub fn is_downloading_shim(exe: &Path) -> bool {
    let resolved = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let in_corepack = |p: &Path| p.components().any(|c| c.as_os_str() == "corepack");
    if in_corepack(&resolved) || in_corepack(exe) {
        return true;
    }
    if resolved
        .file_name()
        .is_some_and(|n| n == "volta-shim" || n == "volta-shim.exe")
    {
        return true;
    }
    // A script that runs corepack: `exec corepack pnpm "$@"`,
    // `node .../corepack/dist/pnpm.js`... (not a mere mention such as
    // `COREPACK_ENABLE_STRICT=0`).
    let mut head = Vec::with_capacity(4096);
    if let Ok(f) = std::fs::File::open(&resolved) {
        let _ = f.take(4096).read_to_end(&mut head);
    }
    if !head.starts_with(b"#!") {
        return false;
    }
    let text = String::from_utf8_lossy(&head);
    text.lines().skip(1).any(|l| {
        let l = l.trim_start();
        !l.starts_with('#')
            && (l.contains("/corepack/")
                || l.starts_with("corepack ")
                || l.contains("exec corepack ")
                || l.contains("/corepack "))
    })
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

    fn script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn corepack_shims_are_never_run() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let bin = root.join("bin");
        let marker = root.join("corepack-ran");
        // The layout `corepack enable` creates: bin/pnpm -> ../lib/node_modules/corepack/dist/pnpm.js
        let shim = root.join("lib/node_modules/corepack/dist/pnpm.js");
        script(
            &shim,
            &format!(
                "#!/bin/sh\ntouch '{}'\necho {}\n",
                marker.display(),
                root.display()
            ),
        );
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(
            "../lib/node_modules/corepack/dist/pnpm.js",
            bin.join("pnpm"),
        )
        .unwrap();
        // A wrapper script handing off to corepack.
        script(
            &bin.join("yarn"),
            &format!(
                "#!/bin/sh\ntouch '{}'\nexec corepack yarn \"$@\"\n",
                marker.display()
            ),
        );
        // A real tool next to them still runs.
        script(&bin.join("realtool"), "#!/bin/sh\necho ok\n");
        // Merely mentioning corepack doesn't make a script a shim.
        script(
            &bin.join("mentions"),
            "#!/bin/sh\n# works with or without corepack\nexport COREPACK_ENABLE_STRICT=0\necho ok\n",
        );

        let tools = Tools::new(
            Some(bin.clone().into_os_string()),
            None,
            DEFAULT_TOOL_TIMEOUT,
        );
        assert!(is_downloading_shim(&bin.join("pnpm")));
        assert!(is_downloading_shim(&bin.join("yarn")));
        assert!(!is_downloading_shim(&bin.join("realtool")));
        assert!(!is_downloading_shim(&bin.join("mentions")));
        assert_eq!(tools.output("mentions", ["x"]).as_deref(), Some("ok"));
        assert!(tools.output("pnpm", ["store", "path"]).is_none());
        assert!(tools.dir_output("pnpm", ["store", "path"]).is_none());
        assert!(tools.output("yarn", ["cache", "dir"]).is_none());
        assert!(!marker.exists(), "a corepack shim was run");
        assert_eq!(tools.output("realtool", ["x"]).as_deref(), Some("ok"));
    }

    #[test]
    fn tools_run_in_root_with_downloads_disabled_and_only_the_given_env() {
        let t = tempfile::tempdir().unwrap();
        let bin = t.path().join("bin");
        script(&bin.join("envtool"), "#!/bin/sh\npwd\n/usr/bin/env\n");
        let tools = Tools::new(
            Some(bin.into_os_string()),
            Some(t.path().join("home")),
            DEFAULT_TOOL_TIMEOUT,
        )
        .with_env(
            [
                ("PNPM_HOME", "/somewhere"),
                // The base environment can't turn the safety switches back on.
                ("COREPACK_ENABLE_NETWORK", "1"),
                ("GOTOOLCHAIN", "auto"),
                ("HOME", "/not/the/scan/home"),
                ("PWD", "/some/project"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.into()))
            .collect::<HashMap<_, _>>()
            .into(),
        );
        let out = tools.output("envtool", ["x"]).unwrap();
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some("/"));
        let vars: std::collections::HashMap<&str, &str> =
            lines.filter_map(|l| l.split_once('=')).collect();
        for (k, v) in SCAN_ENV {
            assert_eq!(vars.get(k), Some(v), "{k}");
        }
        assert_eq!(vars["PNPM_HOME"], "/somewhere");
        assert_ne!(vars.get("PWD"), Some(&"/some/project"));
        assert_eq!(vars["HOME"], t.path().join("home").to_str().unwrap());
        assert!(
            !vars.contains_key("CARGO_MANIFEST_DIR") && !vars.contains_key("CARGO_PKG_NAME"),
            "the test process environment leaked: {vars:?}"
        );
    }
}
