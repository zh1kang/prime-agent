//! One startup hint when the current project has no Captain.
//!
//! The Captain backend publishes its project roots to `captains/roots.json`
//! so this check reads one small file instead of starting Python.

use std::path::{Path, PathBuf};

pub(super) const HINT: &str = "No Captain for this project · F6 n to set one up";

/// The Captain hub, resolved the same way as `captain_backend.py`.
fn hub() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("PA_CAPTAIN_HOME") {
        return Some(PathBuf::from(home));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let defaults = [home.join(".prime/harness"), home.join(".prime/agent")];
    let custom = std::env::var_os("PRIME_AGENT_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .filter(|dir| {
            let dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            !defaults
                .iter()
                .any(|d| d.canonicalize().ok().as_ref() == Some(&dir))
        });
    Some(custom.unwrap_or_else(|| home.join(".prime/harness")))
}

/// The main checkout of a Git project, or the directory itself.
fn project_root(cwd: &Path) -> PathBuf {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "list", "--porcelain", "-z"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stderr(std::process::Stdio::null())
        .output();
    let root = output.ok().filter(|o| o.status.success()).and_then(|o| {
        let first = o.stdout.split(|b| *b == 0).next()?.to_vec();
        let path = first.strip_prefix(b"worktree ")?.to_vec();
        String::from_utf8(path).ok().map(PathBuf::from)
    });
    let root = root.unwrap_or_else(|| cwd.to_path_buf());
    root.canonicalize().unwrap_or(root)
}

/// Whether the project at `cwd` has a Captain, from the published roots.
pub(crate) fn has_captain(cwd: &Path) -> bool {
    hub().is_some_and(|hub| {
        let roots = std::fs::read(hub.join("captains/roots.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<PathBuf>>(&bytes).ok())
            .unwrap_or_default();
        !roots.is_empty() && roots.contains(&project_root(cwd))
    })
}

/// The hint text when `cwd` has no Captain, or `None`.
pub(super) fn missing_captain_hint(cwd: &Path) -> Option<&'static str> {
    missing_in(&hub()?, cwd)
}

fn missing_in(hub: &Path, cwd: &Path) -> Option<&'static str> {
    let captains = hub.join("captains");
    let roots = match std::fs::read(captains.join("roots.json")) {
        Ok(bytes) => serde_json::from_slice::<Vec<PathBuf>>(&bytes).ok()?,
        // A store without the index predates it; the next Captain read writes the index.
        Err(_) if captains.join("projects.sqlite3").exists() => return None,
        Err(_) => Vec::new(),
    };
    let root = project_root(cwd);
    (!roots.contains(&root)).then_some(HINT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captain_hint_reads_the_published_roots() {
        let temp = tempfile::tempdir().unwrap();
        let hub = temp.path().join("hub");
        let project = temp.path().join("project");
        std::fs::create_dir_all(hub.join("captains")).unwrap();
        std::fs::create_dir(&project).unwrap();
        let project = project.canonicalize().unwrap();
        assert_eq!(missing_in(&hub, &project), Some(HINT));
        std::fs::write(hub.join("captains/projects.sqlite3"), b"").unwrap();
        assert_eq!(missing_in(&hub, &project), None);
        std::fs::write(
            hub.join("captains/roots.json"),
            serde_json::to_vec(&[&project]).unwrap(),
        )
        .unwrap();
        assert_eq!(missing_in(&hub, &project), None);
        std::fs::write(hub.join("captains/roots.json"), b"[\"/elsewhere\"]").unwrap();
        assert_eq!(missing_in(&hub, &project), Some(HINT));
    }
}
