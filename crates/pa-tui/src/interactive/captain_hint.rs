//! One startup hint when the current project has no Captain, plus a
//! stat-cached watcher for `<hub>/captains/status/<project-id>.json` so
//! the bottom activity dock and Captain toasts stay live with zero
//! subprocesses per tick.
//!
//! The Captain backend publishes its project roots to `captains/roots.json`
//! and per-project status files to `captains/status/<project-id>.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::captain_view::CrewStates;
use crate::chrome::{CaptainDockActiveItem, CaptainDockState};

pub(super) const HINT: &str = "No Captain for this project · F6 n to set one up";

/// File stamp `(mtime, size, inode)` used to detect status-file changes with a single
/// `stat`. The backend publishes with `os.replace`, which always gives a new inode, so a
/// rewrite within the mtime resolution with the same size still counts as a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
    /// The `SQLite` WAL sidecar's `(mtime, size)`: a WAL-mode commit lands there without
    /// touching the main database file.
    wal: Option<(Option<SystemTime>, u64)>,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some(FileStamp {
        modified: meta.modified().ok(),
        len: meta.len(),
        inode,
        wal: None,
    })
}

/// Project-wide usage totals published in `<hub>/captains/status/<project-id>.json`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct CaptainStatusUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

/// Deserialized `<hub>/captains/status/<project-id>.json`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct CaptainStatusFile {
    pub project_id: String,
    pub name: String,
    pub root: String,
    pub state: String,
    pub lead_state: String,
    pub lead_session_id: String,
    pub usage: CaptainStatusUsage,
    pub session_costs: HashMap<String, f64>,
    pub counts: CaptainStatusCounts,
    pub active: Vec<CaptainStatusActiveItem>,
    pub toast_states: HashMap<String, CaptainToastEntry>,
    /// Unread crew updates across the project.
    pub unread_count: usize,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct CaptainStatusCounts {
    pub working: usize,
    pub needs_you: usize,
    pub factory_running: usize,
    pub factory_paused: usize,
    pub total_crew: usize,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct CaptainStatusActiveItem {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub state: String,
    pub state_text: String,
    pub step: Option<String>,
    pub display_name: String,
    pub unread: bool,
    /// A run's states in flight.
    pub active_states: Vec<String>,
}

/// Where Enter on a `⚓ captain` dock sub-row goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaptainItemTarget {
    /// A Captain-started factory run: the live factory page.
    Run { member: String },
    /// A project factory waiting for review: its review page.
    Factory { name: String },
    /// A crew thread or the Captain: the panel.
    Panel,
}

impl CaptainStatusActiveItem {
    /// The item's destination, from its kind and id (`<project>/<name>` for a factory).
    pub(crate) fn target(&self) -> CaptainItemTarget {
        match self.kind.as_str() {
            "factory" if !self.id.is_empty() => CaptainItemTarget::Run {
                member: self.id.clone(),
            },
            "project_factory" => CaptainItemTarget::Factory {
                name: self
                    .id
                    .rsplit_once('/')
                    .map(|(_, name)| name.to_owned())
                    .unwrap_or_else(|| factory_name(&self.label)),
            },
            _ => CaptainItemTarget::Panel,
        }
    }
}

/// The factory name of a dock label (`factory ship` or `ship`).
pub(crate) fn factory_name(label: &str) -> String {
    label.strip_prefix("factory ").unwrap_or(label).to_owned()
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct CaptainToastEntry {
    pub label: String,
    pub state: String,
    pub kind: String,
    pub display_name: String,
    pub unread: bool,
    pub unread_count: usize,
}

impl CaptainStatusActiveItem {
    /// The dock row's name: the display name, else the label; `• ` marks unread updates.
    fn dock_label(&self, entry: Option<&CaptainToastEntry>) -> String {
        let name = [
            self.display_name.as_str(),
            entry.map_or("", |e| e.display_name.as_str()),
        ]
        .into_iter()
        .find(|name| !name.trim().is_empty())
        .unwrap_or(&self.label);
        if self.unread || entry.is_some_and(|e| e.unread) {
            format!("• {name}")
        } else {
            name.to_owned()
        }
    }

    /// A running run's stage as `▶ implement` (`▶ correctness + bugs` in parallel).
    fn dock_step(&self) -> Option<String> {
        let live = self.kind == "factory" && self.state == "running";
        let stages = if self.active_states.is_empty() {
            self.step.clone().filter(|s| !s.is_empty())?
        } else {
            self.active_states.join(" + ")
        };
        Some(if live {
            format!("▶ {stages}")
        } else {
            stages
        })
    }
}

impl CaptainStatusFile {
    pub(crate) fn to_dock_state(&self) -> CaptainDockState {
        let unread = self.unread_count.max(
            self.toast_states
                .values()
                .map(|entry| entry.unread_count)
                .sum(),
        );
        CaptainDockState {
            project_id: self.project_id.clone(),
            name: if unread > 0 {
                format!("{} •{unread}", self.name)
            } else {
                self.name.clone()
            },
            state: if self.state.is_empty() {
                "active".to_string()
            } else {
                self.state.clone()
            },
            working: self.counts.working,
            needs_you: self.counts.needs_you,
            factory_running: self.counts.factory_running,
            factory_paused: self.counts.factory_paused,
            active: self
                .active
                .iter()
                .map(|item| CaptainDockActiveItem {
                    label: item.dock_label(self.toast_states.get(&item.id)),
                    kind: item.kind.clone(),
                    state: item.state.clone(),
                    state_text: item.state_text.clone(),
                    step: item.dock_step(),
                })
                .collect(),
        }
    }
}

/// Compute Captain change toasts from one project's status file, sharing the
/// exact key space as `captain_view::crew_toasts` so the two never double-fire.
pub(crate) fn status_toasts(
    previous: Option<&CrewStates>,
    status: &CaptainStatusFile,
) -> (CrewStates, Vec<String>) {
    let mut states = previous.cloned().unwrap_or_default();
    let mut toasts = Vec::new();
    let mut keys: Vec<&String> = status.toast_states.keys().collect();
    keys.sort();
    for key in keys {
        let entry = &status.toast_states[key];
        let before = previous.and_then(|p| p.get(key));
        if previous.is_some() && before != Some(&entry.state) {
            if entry.kind == "project_factory" {
                if matches!(entry.state.as_str(), "draft" | "changed") {
                    toasts.push(format!("{} · {} needs review", status.name, entry.label));
                }
            } else {
                let text = match entry.state.as_str() {
                    "needs_you" => Some("needs you"),
                    "done" => Some("done"),
                    "failed" => Some("failed"),
                    _ => None,
                };
                if let Some(text) = text {
                    toasts.push(format!("{} · {} {text}", status.name, entry.label));
                }
            }
        }
        states.insert(key.clone(), entry.state.clone());
    }
    (states, toasts)
}

/// Result of one stat-only poll of the current project's Captain status file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CaptainStatusPoll {
    Unchanged,
    Changed(Option<CaptainStatusFile>),
}

/// Stat-cached reader for `<hub>/captains/status/index.json` and
/// `<hub>/captains/status/<project-id>.json`. Resolves the project root once at
/// construction so subsequent ticks perform only 1–2 `stat` calls and never
/// spawn Python or `git`.
#[derive(Debug)]
pub(crate) struct CaptainStatusWatcher {
    hub: Option<PathBuf>,
    project_root: PathBuf,
    index_stamp: Option<FileStamp>,
    project_id: Option<String>,
    status_stamp: Option<FileStamp>,
    cached: Option<CaptainStatusFile>,
    #[cfg(test)]
    read_count: usize,
}

impl CaptainStatusWatcher {
    pub(crate) fn for_cwd(cwd: &Path) -> Self {
        Self::new(hub(), project_root(cwd))
    }

    pub(crate) fn new(hub: Option<PathBuf>, project_root: PathBuf) -> Self {
        Self {
            hub,
            project_root,
            index_stamp: None,
            project_id: None,
            status_stamp: None,
            cached: None,
            #[cfg(test)]
            read_count: 0,
        }
    }

    /// Whether this project has a Captain, from the published roots; no `git` process.
    pub(crate) fn has_captain(&self) -> bool {
        self.hub.as_ref().is_some_and(|hub| {
            let roots = std::fs::read(hub.join("captains/roots.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Vec<PathBuf>>(&bytes).ok())
                .unwrap_or_default();
            roots.contains(&self.project_root)
        })
    }

    /// The startup hint when this project has no Captain, or `None`; no `git` process.
    pub(crate) fn missing_captain_hint(&self) -> Option<&'static str> {
        missing_in(self.hub.as_ref()?, &self.project_root)
    }

    /// The status file's active item at `index`: the dock's `⚓ captain` sub-row with the
    /// same index, with the id and kind the dock row does not carry.
    pub(crate) fn active_item(&self, index: usize) -> Option<&CaptainStatusActiveItem> {
        self.cached.as_ref()?.active.get(index)
    }

    /// The last parsed status file for this project's Captain, if any.
    pub(crate) fn cached_status(&self) -> Option<&CaptainStatusFile> {
        self.cached.as_ref()
    }

    pub(crate) fn hub_present(&self) -> bool {
        self.hub
            .as_ref()
            .is_some_and(|h| h.join("captains").is_dir())
    }

    /// The file stamps of `captains/projects.sqlite3` and the current
    /// project's status file, so F6 polling skips spawning Python while idle.
    pub(crate) fn db_stamp(&self) -> Option<(Option<FileStamp>, Option<FileStamp>)> {
        let hub = self.hub.as_ref()?;
        let db = file_stamp(&hub.join("captains/projects.sqlite3")).map(|stamp| FileStamp {
            wal: file_stamp(&hub.join("captains/projects.sqlite3-wal"))
                .map(|wal| (wal.modified, wal.len)),
            ..stamp
        });
        let status = self
            .project_id
            .as_ref()
            .and_then(|id| file_stamp(&hub.join("captains/status").join(format!("{id}.json"))));
        Some((db, status))
    }

    /// Re-stat the index and status file; only reads JSON when a stamp changed.
    pub(crate) fn poll(&mut self) -> CaptainStatusPoll {
        let Some(hub) = self.hub.as_ref() else {
            return CaptainStatusPoll::Unchanged;
        };
        let status_dir = hub.join("captains/status");
        let index_path = status_dir.join("index.json");
        let current_index_stamp = file_stamp(&index_path);
        if current_index_stamp != self.index_stamp {
            self.index_stamp = current_index_stamp;
            let new_id = current_index_stamp.and_then(|_| {
                #[cfg(test)]
                {
                    self.read_count += 1;
                }
                let bytes = std::fs::read(&index_path).ok()?;
                let map = serde_json::from_slice::<HashMap<PathBuf, String>>(&bytes).ok()?;
                map.get(&self.project_root).cloned()
            });
            if new_id != self.project_id {
                self.project_id = new_id;
                self.status_stamp = None;
            }
        }
        let Some(id) = self.project_id.as_ref() else {
            if self.cached.take().is_some() {
                self.status_stamp = None;
                return CaptainStatusPoll::Changed(None);
            }
            return CaptainStatusPoll::Unchanged;
        };
        let status_path = status_dir.join(format!("{id}.json"));
        let current_status_stamp = file_stamp(&status_path);
        if current_status_stamp == self.status_stamp {
            return CaptainStatusPoll::Unchanged;
        }
        self.status_stamp = current_status_stamp;
        let parsed = current_status_stamp.and_then(|_| {
            #[cfg(test)]
            {
                self.read_count += 1;
            }
            let bytes = std::fs::read(&status_path).ok()?;
            serde_json::from_slice::<CaptainStatusFile>(&bytes).ok()
        });
        if parsed != self.cached {
            self.cached = parsed.clone();
            CaptainStatusPoll::Changed(parsed)
        } else {
            CaptainStatusPoll::Unchanged
        }
    }
}

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

/// The hint when `root` (a resolved project root) is not among the published roots.
fn missing_in(hub: &Path, root: &Path) -> Option<&'static str> {
    let captains = hub.join("captains");
    let roots = match std::fs::read(captains.join("roots.json")) {
        Ok(bytes) => serde_json::from_slice::<Vec<PathBuf>>(&bytes).ok()?,
        // A store without the index predates it; the next Captain read writes the index.
        Err(_) if captains.join("projects.sqlite3").exists() => return None,
        Err(_) => Vec::new(),
    };
    (!roots.iter().any(|r| r == root)).then_some(HINT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_stamp_changes_when_the_wal_sidecar_grows() {
        let temp = tempfile::tempdir().unwrap();
        let hub = temp.path().join("hub");
        std::fs::create_dir_all(hub.join("captains")).unwrap();
        std::fs::write(hub.join("captains/projects.sqlite3"), b"db").unwrap();
        let watcher = CaptainStatusWatcher::new(Some(hub.clone()), temp.path().to_path_buf());
        let before = watcher.db_stamp();
        std::fs::write(hub.join("captains/projects.sqlite3-wal"), b"commit").unwrap();
        let after = watcher.db_stamp();
        assert_ne!(before, after);
        std::fs::write(hub.join("captains/projects.sqlite3-wal"), b"commit-two").unwrap();
        assert_ne!(after, watcher.db_stamp());
    }

    #[test]
    fn dock_items_keep_their_kind_and_id_for_a_direct_jump() {
        let temp = tempfile::tempdir().unwrap();
        let hub = temp.path().join("hub");
        let project = temp.path().join("project");
        std::fs::create_dir_all(hub.join("captains/status")).unwrap();
        std::fs::create_dir(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let index = serde_json::json!({ project.to_string_lossy().to_string(): "p1" });
        std::fs::write(
            hub.join("captains/status/index.json"),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        let status = serde_json::json!({
            "project_id": "p1", "name": "alpha",
            "active": [
                {"id": "m-gate", "label": "gate-check", "kind": "factory", "state": "paused", "state_text": "paused"},
                {"id": "p1/ship", "label": "factory ship", "kind": "project_factory", "state": "draft", "state_text": "needs review"},
                {"id": "c1", "label": "parser-fix", "kind": "thread", "state": "running", "state_text": "working"}
            ]
        });
        std::fs::write(
            hub.join("captains/status/p1.json"),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
        let mut watcher = CaptainStatusWatcher::new(Some(hub), project);
        let CaptainStatusPoll::Changed(Some(file)) = watcher.poll() else {
            panic!("the status file reads");
        };
        let dock = file.to_dock_state();
        assert_eq!(dock.active.len(), 3, "one dock sub-row per active item");
        assert_eq!(dock.active[1].kind, "project_factory");
        assert_eq!(
            watcher.active_item(0).map(CaptainStatusActiveItem::target),
            Some(CaptainItemTarget::Run {
                member: "m-gate".into()
            })
        );
        assert_eq!(
            watcher.active_item(1).map(CaptainStatusActiveItem::target),
            Some(CaptainItemTarget::Factory {
                name: "ship".into()
            })
        );
        assert_eq!(
            watcher.active_item(2).map(CaptainStatusActiveItem::target),
            Some(CaptainItemTarget::Panel)
        );
        assert_eq!(watcher.active_item(3), None);
        assert_eq!(factory_name("factory ship"), "ship");
        assert_eq!(factory_name("ship"), "ship");
    }

    #[test]
    fn dock_rows_show_display_names_stages_and_unread() {
        let status: CaptainStatusFile = serde_json::from_value(serde_json::json!({
            "project_id": "p1", "name": "eigen", "unread_count": 3,
            "active": [
                {"id": "d0e6edff", "label": "eigen-pipeline", "kind": "factory",
                 "state": "running", "state_text": "working", "step": "implement"},
                {"id": "c1", "label": "parser-fix", "kind": "thread", "state": "running",
                 "state_text": "working"}
            ],
            "toast_states": {
                "d0e6edff": {"label": "eigen-pipeline", "state": "running", "kind": "factory",
                             "display_name": "eigen-pipeline · Phase 2", "unread": true,
                             "unread_count": 2}
            }
        }))
        .unwrap();
        let dock = status.to_dock_state();
        assert_eq!(dock.name, "eigen •3");
        assert_eq!(dock.active[0].label, "• eigen-pipeline · Phase 2");
        assert_eq!(dock.active[0].step.as_deref(), Some("▶ implement"));
        assert_eq!(dock.active[1].label, "parser-fix");
        assert_eq!(dock.active[1].step, None);
    }

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

    #[test]
    fn status_watcher_stats_without_rereading_or_spawning_on_unchanged_ticks() {
        let temp = tempfile::tempdir().unwrap();
        let hub = temp.path().join("hub");
        let project = temp.path().join("project");
        std::fs::create_dir_all(hub.join("captains/status")).unwrap();
        std::fs::create_dir(&project).unwrap();
        let project = project.canonicalize().unwrap();

        let mut watcher = CaptainStatusWatcher::new(Some(hub.clone()), project.clone());
        // No index yet: zero reads.
        assert_eq!(watcher.poll(), CaptainStatusPoll::Unchanged);
        assert_eq!(watcher.read_count, 0);

        // Publish index + status file.
        let index = serde_json::json!({ project.to_string_lossy().to_string(): "p1" });
        std::fs::write(
            hub.join("captains/status/index.json"),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        let status_v1 = serde_json::json!({
            "project_id": "p1",
            "name": "alpha",
            "root": project.to_string_lossy(),
            "state": "active",
            "counts": { "working": 1, "needs_you": 0, "factory_running": 0 },
            "active": [{"id": "c1", "label": "parser-fix", "kind": "thread", "state": "running", "state_text": "working"}],
            "toast_states": {
                "c1": {"label": "parser-fix", "state": "running", "kind": "thread"}
            }
        });
        std::fs::write(
            hub.join("captains/status/p1.json"),
            serde_json::to_vec(&status_v1).unwrap(),
        )
        .unwrap();

        let first = match watcher.poll() {
            CaptainStatusPoll::Changed(Some(s)) => s,
            other => panic!("expected Changed(Some), got {other:?}"),
        };
        assert_eq!(first.counts.working, 1);
        assert_eq!(watcher.read_count, 2, "one read each for index and status");

        // Repeated unchanged ticks must do zero file reads (and zero subprocesses).
        for _ in 0..20 {
            assert_eq!(watcher.poll(), CaptainStatusPoll::Unchanged);
        }
        assert_eq!(watcher.read_count, 2, "no reads on unchanged ticks");

        // First toast pass seeds states without firing.
        let (states, toasts) = status_toasts(None, &first);
        assert!(toasts.is_empty());

        // Update status file with different size so mtime/size stamp changes immediately.
        let status_v2 = serde_json::json!({
            "project_id": "p1",
            "name": "alpha",
            "root": project.to_string_lossy(),
            "state": "active",
            "counts": { "working": 0, "needs_you": 1, "factory_running": 0 },
            "active": [{"id": "c1", "label": "parser-fix", "kind": "thread", "state": "needs_you", "state_text": "needs you"}],
            "toast_states": {
                "c1": {"label": "parser-fix", "state": "needs_you", "kind": "thread"}
            }
        });
        std::fs::write(
            hub.join("captains/status/p1.json"),
            serde_json::to_vec(&status_v2).unwrap(),
        )
        .unwrap();
        let second = match watcher.poll() {
            CaptainStatusPoll::Changed(Some(s)) => s,
            other => panic!("expected Changed(Some), got {other:?}"),
        };
        assert_eq!(
            watcher.read_count, 3,
            "only the changed status file was read"
        );
        let (_, toasts) = status_toasts(Some(&states), &second);
        assert_eq!(toasts, ["alpha · parser-fix needs you"]);
    }
}
