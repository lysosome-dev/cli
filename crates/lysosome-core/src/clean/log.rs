//! The action log: one JSON object per line, appended for every item `clean`
//! acted on, skipped or failed, so you can always see what was removed and
//! where it went.
//!
//! Location: `~/Library/Logs/lysosome/actions.log` on macOS,
//! `$XDG_STATE_HOME/lysosome/actions.log` (default
//! `~/.local/state/lysosome/actions.log`) elsewhere.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::json;

use super::{ItemResult, PlanItem};
use crate::{TOOL_NAME, TOOL_VERSION};

/// Where the action log lives for `home` (and `$XDG_STATE_HOME` off macOS).
pub fn default_log_path(home: &Path, xdg_state_home: Option<&Path>) -> PathBuf {
    if cfg!(target_os = "macos") {
        let _ = xdg_state_home;
        return home
            .join("Library/Logs")
            .join(TOOL_NAME)
            .join("actions.log");
    }
    xdg_state_home
        .filter(|p| p.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local/state"))
        .join(TOOL_NAME)
        .join("actions.log")
}

/// An open, append-only action log.
#[derive(Debug)]
pub struct ActionLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl ActionLog {
    /// Open (creating folders as needed) for appending.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(ActionLog {
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry and flush it to disk.
    pub fn record(&self, item: &PlanItem, result: &ItemResult) -> std::io::Result<()> {
        let entry = json!({
            "ts": humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
            "tool_version": TOOL_VERSION,
            "id": item.id,
            "category": item.category,
            "kind": item.kind,
            "action": item.action,
            "target": item.path.to_string_lossy(),
            "command": item.command,
            "paths": item.paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "size_bytes": item.size_bytes,
            "freed_bytes": result.freed_bytes,
            "outcome": result.outcome,
            "destination": result.destination,
            "message": result.message,
        });
        let mut line = serde_json::to_vec(&entry).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
        f.write_all(&line)?;
        f.flush()?;
        f.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_per_platform() {
        let home = Path::new("/home/u");
        let p = default_log_path(home, Some(Path::new("/state")));
        if cfg!(target_os = "macos") {
            assert_eq!(p, Path::new("/home/u/Library/Logs/lysosome/actions.log"));
        } else {
            assert_eq!(p, Path::new("/state/lysosome/actions.log"));
            assert_eq!(
                default_log_path(home, None),
                Path::new("/home/u/.local/state/lysosome/actions.log")
            );
            assert_eq!(
                default_log_path(home, Some(Path::new("relative"))),
                Path::new("/home/u/.local/state/lysosome/actions.log")
            );
        }
    }
}
