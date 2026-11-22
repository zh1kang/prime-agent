//! The thread's workspace binding: which checkout a session works in after a worktree move.
//!
//! Session files are append-only and their header `cwd` is the creation directory, so the
//! binding lives beside them as `session-artifacts/<id>/workspace.json`. Every reopen path
//! (worker create, recovery respawn, session switch) resolves the working directory through
//! [`effective_session_cwd`]: the binding's cwd when one is recorded, else the header's.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// File name of the binding inside the session artifact dir.
pub const WORKSPACE_BINDING_FILE: &str = "workspace.json";

/// One recorded workspace binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceBinding {
    /// The directory the thread works in.
    pub cwd: String,
    /// The main checkout of the repository the thread belongs to.
    pub main_checkout: String,
    /// The linked worktree the thread works in; `None` in the main checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// The branch checked out in `cwd` when the binding was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The revision a created worktree started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    /// ISO-8601 time of the move.
    pub updated_at: String,
}

/// The binding file of the session stored at `session_file`.
#[must_use]
pub fn binding_path(session_file: &Path) -> Option<PathBuf> {
    crate::session_engine::harness_digest::session_artifact_dir_for_log(session_file)
        .map(|dir| dir.join(WORKSPACE_BINDING_FILE))
}

/// The recorded binding; `None` when absent or unreadable (a damaged file falls back to the
/// header cwd rather than blocking the reopen).
#[must_use]
pub fn read_binding(session_file: &Path) -> Option<WorkspaceBinding> {
    let path = binding_path(session_file)?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist the binding with a write-then-rename, so a crash never leaves a partial file.
///
/// # Errors
///
/// The I/O error when the artifact dir or file cannot be written.
pub fn write_binding(session_file: &Path, binding: &WorkspaceBinding) -> Result<()> {
    let path = binding_path(session_file).context("Session file has no artifact directory")?;
    let dir = path.parent().context("Binding path has no parent")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Could not create {}", dir.display()))?;
    let temp = dir.join(format!(".{WORKSPACE_BINDING_FILE}.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temp, serde_json::to_vec_pretty(binding)?)
        .with_context(|| format!("Could not write {}", temp.display()))?;
    crate::platform::rename_onto(&temp, &path)
        .with_context(|| format!("Could not replace {}", path.display()))?;
    Ok(())
}

/// The working directory a reopen of `session_file` uses: the binding's cwd while that
/// directory exists, else the header cwd. A removed worktree therefore reopens in the
/// header's checkout instead of failing.
#[must_use]
pub fn effective_session_cwd(session_file: &Path) -> Option<String> {
    if let Some(binding) = read_binding(session_file) {
        if Path::new(&binding.cwd).is_dir() {
            return Some(binding.cwd);
        }
    }
    crate::session::manager::read_session_header(session_file)
        .map(|header| header.cwd)
        .filter(|cwd| !cwd.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_file(root: &Path, header_cwd: &Path) -> PathBuf {
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let file = sessions.join("2026-11-21_abc.jsonl");
        let header = serde_json::json!({
            "type": "session", "version": 3, "id": "abc",
            "timestamp": "2026-11-21T00:00:00.000Z", "cwd": header_cwd,
        });
        std::fs::write(&file, format!("{header}\n")).unwrap();
        file
    }

    #[test]
    fn the_binding_cwd_wins_while_it_exists_and_falls_back_after_removal() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        let file = session_file(root.path(), &main);
        assert_eq!(effective_session_cwd(&file), Some(main.display().to_string()));

        let binding = WorkspaceBinding {
            cwd: worktree.display().to_string(),
            main_checkout: main.display().to_string(),
            worktree_path: Some(worktree.display().to_string()),
            branch: Some("prime/feature".to_string()),
            base_ref: Some("main".to_string()),
            updated_at: "2026-11-21T00:00:00.000Z".to_string(),
        };
        write_binding(&file, &binding).unwrap();
        assert_eq!(read_binding(&file), Some(binding));
        assert_eq!(effective_session_cwd(&file), Some(worktree.display().to_string()));

        std::fs::remove_dir_all(&worktree).unwrap();
        assert_eq!(effective_session_cwd(&file), Some(main.display().to_string()));
    }
}
