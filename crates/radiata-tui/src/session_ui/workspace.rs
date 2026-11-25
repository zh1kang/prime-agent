//! The workspace pages' session surface: history, the machine editor, and git review open as
//! a native overlay, their `pa thread` calls run in the background, text steps use pa's prompt
//! editor, and the optional "edit file in $EDITOR" key hands the terminal to the editor and
//! returns.

use std::path::{Path, PathBuf};
use std::process::Command;

use radiata_types::daemon::DaemonCommand;
use serde_json::{json, Map, Value};

use super::{AgentView, Overlay};
use crate::interactive::SessionSelection;
use crate::session_ui::CaptainUpdate;
use crate::workspace_view::{compact_count, WorkspaceAction, WorkspaceRequest, WorkspaceView};

impl super::SessionUi {
    /// Open a workspace page with its first call.
    pub(crate) fn open_workspace(
        &mut self,
        (mut page, action): (WorkspaceView, WorkspaceAction),
        view: &mut AgentView,
    ) {
        if let Some(model) = self.current_model(view) {
            page.set_host_model(format!("{}/{}", model.provider, model.model_id));
        }
        let findings = page.findings_call();
        view.overlay = Some(Overlay::Workspace(page));
        self.dirty = true;
        self.run_workspace_action(action, view);
        self.run_workspace_action(findings, view);
    }

    pub(crate) fn handle_workspace_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        view: &mut AgentView,
    ) {
        let Some(id) = super::key_event_to_id(&key) else {
            return;
        };
        let kb = view.editor.keybindings().clone();
        let action = match view.overlay.as_mut() {
            Some(Overlay::Workspace(page)) => page.handle_key(&id, &kb),
            _ => return,
        };
        self.dirty = true;
        self.run_workspace_action(action, view);
    }

    pub(crate) fn run_workspace_action(&mut self, action: WorkspaceAction, view: &mut AgentView) {
        match action {
            WorkspaceAction::None => {}
            WorkspaceAction::Close => {
                let page = match view.overlay.take() {
                    Some(Overlay::Workspace(page)) => Some(page),
                    other => {
                        view.overlay = other;
                        None
                    }
                };
                self.dirty = true;
                if let Some(mut page) = page.filter(WorkspaceView::returns_to_captain) {
                    // Back to the Captain: its factory review page reloads the factory; a crew
                    // review lands on the crew row it was opened from.
                    let notice = page.take_notice();
                    self.open_captain_panel(view, "");
                    if let Some(Overlay::Captain(panel)) = view.overlay.as_mut() {
                        if let Some(notice) = notice {
                            panel.set_notice(&notice);
                        }
                    }
                    if page.edits_captain_factory() {
                        self.captain_file_edited(view);
                    }
                }
            }
            WorkspaceAction::Compose { label, initial } => {
                if let Some(Overlay::Workspace(page)) = view.overlay.take() {
                    self.start_compose(
                        super::captain::ComposeOwner::Workspace(Box::new(page)),
                        label,
                        &initial,
                        view,
                    );
                }
            }
            WorkspaceAction::Call {
                request,
                args,
                input,
            } => self.spawn_thread(args, Some(input), move |result| CaptainUpdate::Workspace {
                request,
                result,
            }),
            WorkspaceAction::Resume(path) => {
                view.overlay = None;
                let path = PathBuf::from(path);
                if path.is_file() {
                    self.pending_selection = Some(SessionSelection::Resume(path));
                    self.exit_requested = true;
                } else {
                    self.error_row("Selected session disappeared", view);
                }
            }
            WorkspaceAction::MergeWorktree(worktree) => {
                let tx = self.captain_updates.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || merge_worktree(&worktree))
                        .await
                        .unwrap_or_else(|e| Err(format!("Merge failed: {e}")))
                        .map(|message| json!({ "message": message }));
                    let _ = tx.send(CaptainUpdate::Workspace {
                        request: WorkspaceRequest::Merge,
                        result,
                    });
                });
            }
            WorkspaceAction::Absorb { sources, focus } => {
                if matches!(view.overlay, Some(Overlay::Workspace(_))) {
                    view.overlay = None;
                }
                self.toast(&format!("Absorbing {}…", chats(sources.len())), view);
                let client = self.client.clone();
                let active_session_id = self.active_session_id.clone();
                let tx = self.captain_updates.clone();
                tokio::spawn(async move {
                    let result = client
                        .request_ok(DaemonCommand::AbsorbContext {
                            id: None,
                            active_session_id,
                            sources,
                            focus,
                            budget_tokens: None,
                            rest: Map::default(),
                        })
                        .await
                        .map_err(|error| format!("{error:#}"));
                    let _ = tx.send(CaptainUpdate::Workspace {
                        request: WorkspaceRequest::Absorb,
                        result,
                    });
                });
            }
            WorkspaceAction::EditFile(path) => {
                // The one screen outside pa, by an explicit optional key: the user's editor on a
                // library machine's definition file.
                self.workspace_command = Some(("edit-file", path));
            }
            WorkspaceAction::PickModel {
                draft,
                state_id,
                role,
                current_model,
                current_thinking,
            } => {
                if let Some(Overlay::Workspace(page)) = view.overlay.take() {
                    self.workspace_parked = Some(page);
                }
                self.open_shared_model_picker(
                    view,
                    &current_model,
                    &current_thinking,
                    super::model_picker::ModelPickTarget::Workspace {
                        draft,
                        state_id,
                        role,
                        current_thinking: current_thinking.clone(),
                    },
                );
            }
        }
    }

    pub(crate) fn apply_workspace_update(
        &mut self,
        request: WorkspaceRequest,
        result: Result<Value, String>,
        view: &mut AgentView,
    ) {
        self.dirty = true;
        if request == WorkspaceRequest::Absorb {
            // The daemon appends the `absorbed_context` card itself; this only reports.
            match result {
                Ok(data) => self.toast(&absorbed_summary(&data), view),
                Err(error) => self.error_row(&format!("Absorb failed: {error}"), view),
            }
            return;
        }
        let action = match view.overlay.as_mut() {
            Some(Overlay::Workspace(page)) => page.apply(request, result),
            // The page closed before its answer arrived.
            _ => return,
        };
        self.run_workspace_action(action, view);
    }

    /// The `$EDITOR` handoff returned: the open page re-reads what changed.
    pub(crate) fn workspace_file_edited(&mut self, view: &mut AgentView) {
        let action = match view.overlay.as_mut() {
            Some(Overlay::Workspace(page)) => page.file_edited(),
            Some(Overlay::Captain(_)) => return self.captain_file_edited(view),
            _ => return,
        };
        self.dirty = true;
        self.run_workspace_action(action, view);
    }
}

fn chats(n: usize) -> String {
    format!("{n} chat{}", if n == 1 { "" } else { "s" })
}

/// `Absorbed 1 chat (1.4k tok · distill) · 1 file changed since` from an `absorb_context` answer.
fn absorbed_summary(data: &Value) -> String {
    let count = data["sources"].as_array().map_or(1, Vec::len);
    let tokens = compact_count(data["tokens"].as_u64().unwrap_or(0));
    let mut text = format!("Absorbed {} ({tokens} tok", chats(count));
    if let Some(method) = data["method"].as_str().filter(|m| !m.is_empty()) {
        text.push_str(&format!(" · {method}"));
    }
    text.push(')');
    let changed = data["changedFiles"].as_array().map_or(0, Vec::len);
    if changed > 0 {
        let plural = if changed == 1 { "" } else { "s" };
        text.push_str(&format!(" · {changed} file{plural} changed since"));
    }
    text
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("Could not run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

/// Merge the branch checked out in a crew worktree into the project's main checkout. Both
/// checkouts must be clean, so the merge never touches uncommitted work.
fn merge_worktree(worktree: &str) -> Result<String, String> {
    let worktree = Path::new(worktree);
    let branch = git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        return Err("Crew worktree has a detached HEAD; nothing to merge.".into());
    }
    let common = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let main_root = Path::new(&common)
        .parent()
        .ok_or_else(|| format!("No main checkout above {common}"))?
        .to_path_buf();
    if main_root == worktree {
        return Err("This review is the main checkout, not a crew worktree.".into());
    }
    if !git(worktree, &["status", "--porcelain"])?.is_empty() {
        return Err(format!(
            "Crew worktree has uncommitted changes; commit them before merging {branch}."
        ));
    }
    if !git(
        &main_root,
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty()
    {
        return Err(format!(
            "Main checkout {} has uncommitted changes; merge cancelled.",
            main_root.display()
        ));
    }
    git(&main_root, &["merge", "--no-edit", &branch])?;
    Ok(format!("Merged {branch} into main."))
}
