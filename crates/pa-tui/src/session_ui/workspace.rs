//! The workspace pages' session surface: history, the machine editor, and git review open as
//! a native overlay, their `pa thread` calls run in the background, text steps use pa's prompt
//! editor, and the optional "edit file in $EDITOR" key hands the terminal to the editor and
//! returns.

use std::path::PathBuf;

use serde_json::Value;

use super::{AgentView, Overlay};
use crate::interactive::SessionSelection;
use crate::session_ui::CaptainUpdate;
use crate::workspace_view::{WorkspaceAction, WorkspaceRequest, WorkspaceView};

impl super::SessionUi {
    /// Open a workspace page with its first call.
    pub(crate) fn open_workspace(
        &mut self,
        (page, action): (WorkspaceView, WorkspaceAction),
        view: &mut AgentView,
    ) {
        view.overlay = Some(Overlay::Workspace(page));
        self.dirty = true;
        self.run_workspace_action(action, view);
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
                    // Back to the Captain's factory review page, which reloads the factory.
                    let notice = page.take_notice();
                    self.open_captain_panel(view, "");
                    if let Some(Overlay::Captain(panel)) = view.overlay.as_mut() {
                        if let Some(notice) = notice {
                            panel.set_notice(&notice);
                        }
                    }
                    self.captain_file_edited(view);
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
            WorkspaceAction::EditFile(path) => {
                // The one screen outside pa, by an explicit optional key: the user's editor on a
                // library machine's definition file.
                self.workspace_command = Some(("edit-file", path));
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
