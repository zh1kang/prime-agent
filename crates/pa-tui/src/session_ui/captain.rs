//! The Captain panel's session surface: the panel open (F6, `/captain`), its key loop, the
//! `pa thread captain` calls its actions ride, text steps in pa's prompt editor, in-place
//! switching between a Captain and its crew chats, and the breadcrumb above the prompt.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{AgentView, Overlay};
use crate::captain_view::{
    CaptainAction, CaptainSnapshot, CaptainView, ComposePurpose, OpenTarget,
};
use crate::interactive::SessionSelection;
use crate::session_ui::picker_viewport_rows;
use crate::session_ui::sessions_fork::create_session;

/// One background `pa thread captain` call's result.
pub(crate) enum CaptainUpdate {
    Snapshot(Result<Value, String>),
    Acted {
        op: String,
        done: String,
        result: Result<Value, String>,
    },
    Choices {
        project: Option<String>,
        result: Result<Value, String>,
    },
    Opened(Result<Value, String>),
    Transcript {
        project: String,
        member: String,
        result: Result<Value, String>,
    },
    /// A project factory's `factory-show` reply, for its review page.
    Factory {
        project: String,
        /// The user asked for this load; a poll is not explicit.
        explicit: bool,
        result: Result<Value, String>,
    },
    /// A workspace page's `pa thread` call.
    Workspace {
        request: crate::workspace_view::WorkspaceRequest,
        result: Result<Value, String>,
    },
}

/// A text step waiting in pa's prompt editor; the chat's own draft waits beside it.
pub(crate) struct CaptainCompose {
    owner: ComposeOwner,
    label: String,
    saved_draft: String,
}

/// The page a text step returns to with its answer.
pub(crate) enum ComposeOwner {
    /// A Captain step; the panel is none for a step started from a slash command.
    Captain {
        purpose: ComposePurpose,
        panel: Option<CaptainView>,
    },
    /// A workspace page step (machine editor names, models, and definition files).
    Workspace(Box<crate::workspace_view::WorkspaceView>),
}

/// The Captain project the open chat belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptainContext {
    pub project: String,
    pub project_name: String,
    /// The open chat's label: "captain" or the crew member's title.
    pub chat: String,
    /// Every chat of the project that pa can open: (label, session path, member id).
    pub chats: Vec<CrewChat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrewChat {
    pub label: String,
    pub session: Option<String>,
    pub member: Option<String>,
    pub busy: bool,
}

fn same_file(a: &str, b: &str) -> bool {
    let canonical = |p: &str| {
        Path::new(p)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(p))
    };
    a == b || canonical(a) == canonical(b)
}

/// The project and chat label for `session_file`, from a status snapshot.
pub(crate) fn captain_context(
    snapshot: &CaptainSnapshot,
    session_file: &str,
) -> Option<CaptainContext> {
    for project in &snapshot.projects {
        let mut chats = vec![CrewChat {
            label: "captain".into(),
            session: project.lead_session.clone(),
            member: None,
            busy: project.lead_busy,
        }];
        chats.extend(
            project
                .crew
                .iter()
                .filter(|m| m.kind != "factory")
                .map(|m| CrewChat {
                    label: m.label(),
                    session: m.session.clone(),
                    member: Some(m.id.clone()),
                    busy: m.busy,
                }),
        );
        let current = chats.iter().find(|chat| {
            chat.session
                .as_deref()
                .is_some_and(|path| same_file(path, session_file))
        });
        if let Some(current) = current {
            return Some(CaptainContext {
                project: project.id.clone(),
                project_name: project.name.clone(),
                chat: current.label.clone(),
                chats,
            });
        }
    }
    None
}

/// The breadcrumb above the prompt in a Captain chat.
pub(crate) fn breadcrumb(context: &CaptainContext, switch_key: &str, parent_key: &str) -> String {
    let path = if context.chat == "captain" {
        format!("{} › captain", context.project_name)
    } else {
        format!("captain › {}", context.chat)
    };
    format!("{path} · {switch_key} switch chat · {parent_key} captain")
}

impl super::SessionUi {
    fn key_text(view: &AgentView, id: &str) -> String {
        crate::keybindings::format_key_text(&view.editor.keybindings().get_keys(id).join("/"))
    }

    /// Run `pa thread captain ARGS` in the background; `map` turns its JSON into an update.
    fn spawn_captain(
        &self,
        args: Vec<String>,
        input: Option<Value>,
        map: impl FnOnce(Result<Value, String>) -> CaptainUpdate + Send + 'static,
    ) {
        let mut thread_args = vec!["captain".to_owned()];
        thread_args.extend(args);
        self.spawn_thread(thread_args, input, map);
    }

    /// Run `pa thread ARGS` in the background with `input` as JSON on stdin; `map` turns its
    /// JSON answer into an update.
    pub(crate) fn spawn_thread(
        &self,
        args: Vec<String>,
        input: Option<Value>,
        map: impl FnOnce(Result<Value, String>) -> CaptainUpdate + Send + 'static,
    ) {
        let tx = self.captain_updates.clone();
        let cwd = self.cwd.clone();
        let session = self.session_file.clone();
        tokio::spawn(async move {
            let result = async {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let mut command = tokio::process::Command::new(exe);
                command
                    .arg("thread")
                    .args(&args)
                    .current_dir(&cwd)
                    .env_remove("PA_CAPTAIN_ACTOR")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);
                if let Some(session) = &session {
                    command.env("PA_CAPTAIN_SESSION", session);
                }
                let mut child = command
                    .spawn()
                    .map_err(|e| format!("Could not start pa: {e}"))?;
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    let body = input.map(|v| v.to_string()).unwrap_or_default();
                    stdin
                        .write_all(body.as_bytes())
                        .await
                        .map_err(|e| e.to_string())?;
                }
                let output = child.wait_with_output().await.map_err(|e| e.to_string())?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let message = stderr.trim().trim_start_matches("Captain: ").trim();
                    return Err(if message.is_empty() {
                        format!("Captain exited with {}", output.status)
                    } else {
                        message.lines().last().unwrap_or(message).to_owned()
                    });
                }
                serde_json::from_slice::<Value>(&output.stdout)
                    .map_err(|e| format!("Captain returned invalid JSON: {e}"))
            }
            .await;
            let _ = tx.send(map(result));
        });
    }

    fn captain_act(&self, op: &str, args: Value, done: String) {
        let op_name = op.to_owned();
        self.spawn_captain(vec!["act".into(), op.into()], Some(args), move |result| {
            CaptainUpdate::Acted {
                op: op_name,
                done,
                result,
            }
        });
    }

    pub(crate) fn refresh_captain(&self) {
        self.spawn_captain(vec!["status".into()], None, CaptainUpdate::Snapshot);
    }

    /// The open panel's poll: the records, and the open provider thread's transcript.
    pub(crate) fn poll_captain(&self, view: &AgentView) {
        if let Some(Overlay::Captain(panel)) = &view.overlay {
            if let Some((project, member)) = panel.open_thread_member() {
                self.load_transcript(project, member);
            }
            if let Some((project, name)) = panel.open_factory_member() {
                self.load_factory(project, name, false);
            }
        }
        self.refresh_captain();
    }

    /// Read the Captain records when this chat may belong to a project, for the breadcrumb.
    pub(crate) fn refresh_captain_context(&self) {
        if self.session_file.is_some() && crate::interactive::captain_hint::has_captain(&self.cwd) {
            self.refresh_captain();
        }
    }

    /// F6 and `/captain [name|new|promote]`: open the native Captain panel.
    pub(crate) fn open_captain_panel(&mut self, view: &mut AgentView, query: &str) {
        let mut panel = self
            .captain_parked
            .take()
            .unwrap_or_else(|| CaptainView::new(picker_viewport_rows(view.terminal_rows()) + 8));
        let query = query.trim().to_owned();
        let action = match query.as_str() {
            "new" => panel.start_setup(None),
            "promote" => panel.start_setup(Some("current".into())),
            _ => CaptainAction::None,
        };
        if !query.is_empty() && !matches!(query.as_str(), "new" | "promote") {
            self.captain_focus_query = Some(query);
        }
        view.overlay = Some(Overlay::Captain(panel));
        self.refresh_captain();
        self.dirty = true;
        self.run_captain_action(action, view);
    }

    pub(crate) fn handle_captain_view_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        view: &mut AgentView,
    ) {
        let Some(id) = super::key_event_to_id(&key) else {
            return;
        };
        let action = match view.overlay.as_mut() {
            Some(Overlay::Captain(panel)) => panel.handle_key(&id, view.editor.keybindings()),
            _ => return,
        };
        self.dirty = true;
        self.run_captain_action(action, view);
    }

    fn panel_mut(view: &mut AgentView) -> Option<&mut CaptainView> {
        match view.overlay.as_mut() {
            Some(Overlay::Captain(panel)) => Some(panel),
            _ => None,
        }
    }

    fn run_captain_action(&mut self, action: CaptainAction, view: &mut AgentView) {
        match action {
            CaptainAction::None => {}
            CaptainAction::Close => {
                view.overlay = None;
                self.captain_parked = None;
                self.dirty = true;
            }
            CaptainAction::Refresh => self.refresh_captain(),
            CaptainAction::Act { op, args, done } => {
                if let Some(panel) = Self::panel_mut(view) {
                    panel.set_notice("Working…");
                }
                self.captain_act(&op, args, done);
            }
            CaptainAction::Open { project, member } => {
                if let Some(panel) = Self::panel_mut(view) {
                    panel.set_notice("Opening…");
                }
                let args = serde_json::json!({
                    "project": project,
                    "member": member.unwrap_or_else(|| "lead".into()),
                });
                self.spawn_captain(
                    vec!["act".into(), "open".into()],
                    Some(args),
                    CaptainUpdate::Opened,
                );
            }
            CaptainAction::Compose {
                purpose,
                label,
                initial,
            } => self.start_captain_compose(purpose, label, &initial, view),
            CaptainAction::LoadChoices { project, reference } => {
                let mut args = serde_json::json!({});
                if let Some(project) = &project {
                    args["project"] = Value::String(project.clone());
                }
                if let Some(reference) = reference {
                    args["ref"] = Value::String(reference);
                }
                self.spawn_captain(
                    vec!["act".into(), "choices".into()],
                    Some(args),
                    move |result| CaptainUpdate::Choices { project, result },
                );
            }
            CaptainAction::LoadFactory { project, name } => {
                self.load_factory(project, name, true);
            }
            CaptainAction::EditFactory { root, name } => {
                // pa's machine editor on the project factory; closing it returns to the review
                // page, which reloads the factory (a saved edit needs approval again).
                if let Some(Overlay::Captain(panel)) = view.overlay.take() {
                    self.captain_parked = Some(panel);
                }
                self.open_workspace(
                    crate::workspace_view::WorkspaceView::project_machine(&root, &name),
                    view,
                );
            }
            CaptainAction::Review { project, member } => {
                // The review page replaces the panel; Esc returns to the chat.
                if let Some(Overlay::Captain(panel)) = view.overlay.take() {
                    self.captain_parked = Some(panel);
                }
                self.open_workspace(
                    crate::workspace_view::WorkspaceView::review(Some((project, member))),
                    view,
                );
            }
        }
    }

    /// A message typed in the Captain's own chat becomes a Captain turn (a `lead` job), so the
    /// Captain reads its rules, plan, crew reports, and operations, and the backend counts it as
    /// a user turn for approvals and grants. The job runs through this chat's daemon, so the
    /// message and reply appear here. Returns true when the message was taken.
    pub(crate) fn send_to_captain(&mut self, text: &str, view: &mut AgentView) -> bool {
        let Some(project) = self
            .captain_context
            .as_ref()
            .filter(|context| context.chat == "captain")
            .map(|context| context.project.clone())
        else {
            return false;
        };
        if !crate::image_markers::collect_marked_images(&self.pasted_images, text).is_empty() {
            view.editor.set_text(text);
            self.note(
                "The Captain takes text only. Remove the image to send.",
                view,
            );
            return true;
        }
        view.editor.add_to_history(text);
        self.captain_act(
            "lead",
            serde_json::json!({"project": project, "text": text}),
            String::new(),
        );
        true
    }

    /// A text step: park the panel and the chat's draft, and ask in pa's prompt editor.
    fn start_captain_compose(
        &mut self,
        purpose: ComposePurpose,
        label: String,
        initial: &str,
        view: &mut AgentView,
    ) {
        let panel = match view.overlay.take() {
            Some(Overlay::Captain(panel)) => Some(panel),
            other => {
                view.overlay = other;
                None
            }
        };
        self.start_compose(
            ComposeOwner::Captain { purpose, panel },
            label,
            initial,
            view,
        );
    }

    /// Hand pa's prompt editor to a text step; the chat's draft is kept and restored after.
    pub(crate) fn start_compose(
        &mut self,
        owner: ComposeOwner,
        label: String,
        initial: &str,
        view: &mut AgentView,
    ) {
        let saved_draft = view.editor.get_expanded_text();
        view.editor.set_text(initial);
        self.captain_compose = Some(CaptainCompose {
            owner,
            label,
            saved_draft,
        });
        self.sync_captain_header(view);
        self.dirty = true;
    }

    /// Whether a Captain text step owns the editor's submit and cancel.
    pub(crate) fn captain_compose_active(&self) -> bool {
        self.captain_compose.is_some()
    }

    /// The editor's submit or Esc during a text step. Returns true when the key was taken.
    pub(crate) fn captain_compose_key(&mut self, id: &str, view: &mut AgentView) -> bool {
        let kb = view.editor.keybindings();
        let submit = kb.matches(id, "tui.input.submit");
        let cancel = id == "escape" || kb.matches(id, "app.interrupt");
        if !submit && !cancel {
            return false;
        }
        let Some(compose) = self.captain_compose.take() else {
            return false;
        };
        let text = submit.then(|| view.editor.get_expanded_text());
        view.editor.set_text(&compose.saved_draft);
        self.dirty = true;
        match compose.owner {
            ComposeOwner::Captain { purpose, panel } => {
                let mut panel = panel.unwrap_or_else(|| {
                    CaptainView::new(view.terminal_rows().saturating_sub(4) as usize)
                });
                let action = panel.compose_done(purpose, text);
                view.overlay = Some(Overlay::Captain(panel));
                self.sync_captain_header(view);
                self.run_captain_action(action, view);
            }
            ComposeOwner::Workspace(mut page) => {
                let action = page.compose_done(text);
                view.overlay = Some(Overlay::Workspace(*page));
                self.sync_captain_header(view);
                self.run_workspace_action(action, view);
            }
        }
        true
    }

    /// The line above the prompt: the text step's label, else the Captain breadcrumb.
    pub(crate) fn sync_captain_header(&mut self, view: &mut AgentView) {
        let submit = Self::key_text(view, "tui.input.submit");
        view.editor_header = if let Some(compose) = &self.captain_compose {
            Some(format!("{} · {submit} done · Esc cancel", compose.label))
        } else {
            self.captain_context.as_ref().map(|context| {
                breadcrumb(
                    context,
                    &Self::key_text(view, "app.captain.crew"),
                    &Self::key_text(view, "app.captain.parent"),
                )
            })
        };
    }

    pub(crate) fn apply_captain_update(&mut self, update: CaptainUpdate, view: &mut AgentView) {
        self.dirty = true;
        match update {
            CaptainUpdate::Snapshot(result) => {
                if let Ok(data) = &result {
                    if let Ok(snapshot) = serde_json::from_value::<CaptainSnapshot>(data.clone()) {
                        self.captain_context = self
                            .session_file
                            .as_deref()
                            .and_then(|file| captain_context(&snapshot, file));
                        self.sync_captain_header(view);
                        let (states, toasts) = crate::captain_view::crew_toasts(
                            self.captain_states.as_ref(),
                            &snapshot,
                        );
                        self.captain_states = Some(states);
                        for toast in toasts {
                            view.toasts.push(toast);
                        }
                    }
                }
                if let Some(panel) = Self::panel_mut(view) {
                    match result {
                        Ok(data) => {
                            panel.apply_snapshot(&data);
                            if let Some(query) = self.captain_focus_query.take() {
                                if !panel.focus_project(&query) {
                                    panel.set_notice(format!("No project matches {query}."));
                                }
                            }
                            if let Some(key) = self.captain_pending_key.take() {
                                let here = panel.snapshot().current_root.clone();
                                if panel.focus_project(&here) {
                                    let kb = view.editor.keybindings().clone();
                                    let action = match view.overlay.as_mut() {
                                        Some(Overlay::Captain(panel)) => panel.handle_key(key, &kb),
                                        _ => CaptainAction::None,
                                    };
                                    self.run_captain_action(action, view);
                                } else {
                                    panel.set_notice("This project has no Captain.");
                                }
                            }
                        }
                        Err(error) => panel.set_snapshot_error(error),
                    }
                }
                if let Some(Overlay::CrewPicker(picker)) = view.overlay.as_mut() {
                    if let Some(context) = &self.captain_context {
                        picker.set_chats(context.chats.clone());
                    }
                }
            }
            CaptainUpdate::Acted { op, done, result } => match result {
                Ok(reply) => {
                    if let Some(target) = OpenTarget::from_reply(&reply) {
                        self.open_captain_target(target, view);
                        return;
                    }
                    if matches!(op.as_str(), "init" | "promote") {
                        // The new project is this checkout's: select it.
                        self.captain_focus_query = Self::panel_mut(view)
                            .map(|panel| panel.snapshot().current_root.clone());
                    }
                    let mut notice = done;
                    if op == "stop" && reply["state"] == "stopping" {
                        notice = "Stopping. Running work is ending.".into();
                    }
                    if op == "delete" && reply["state"] != "deleted" {
                        notice = "Deleting. Running work is ending.".into();
                    }
                    if let Some(error) = reply.get("factory_error").and_then(Value::as_str) {
                        notice = format!("{notice} The factory did not start: {error}");
                    }
                    if let Some(errors) = reply.get("crew_errors").and_then(Value::as_array) {
                        notice = format!("{notice} {} thread(s) could not join.", errors.len());
                    }
                    match Self::panel_mut(view) {
                        Some(panel) => {
                            panel.set_error(None);
                            panel.set_notice(notice);
                        }
                        None if !notice.is_empty() => self.note(&notice, view),
                        None => {}
                    }
                    self.refresh_captain();
                    if let Some((project, member)) =
                        Self::panel_mut(view).and_then(|p| p.open_thread_member())
                    {
                        self.load_transcript(project, member);
                    }
                    if let Some((project, name)) =
                        Self::panel_mut(view).and_then(|p| p.open_factory_member())
                    {
                        self.load_factory(project, name, false);
                    }
                }
                Err(error) => match Self::panel_mut(view) {
                    Some(panel) => panel.set_error(Some(error)),
                    None => self.error_row(&error, view),
                },
            },
            CaptainUpdate::Choices { project, result } => {
                let action = match (Self::panel_mut(view), result) {
                    (Some(panel), Ok(data)) => panel.apply_choices(&data, project.as_deref()),
                    (Some(panel), Err(error)) => {
                        panel.set_error(Some(error));
                        CaptainAction::None
                    }
                    (None, _) => CaptainAction::None,
                };
                self.run_captain_action(action, view);
            }
            CaptainUpdate::Opened(result) => match result {
                Ok(reply) => match OpenTarget::from_reply(&reply) {
                    Some(target) => self.open_captain_target(target, view),
                    None => {
                        if let Some(panel) = Self::panel_mut(view) {
                            panel.set_error(Some("Captain returned nothing to open.".into()));
                        }
                    }
                },
                Err(error) => match Self::panel_mut(view) {
                    Some(panel) => panel.set_error(Some(error)),
                    None => self.error_row(&error, view),
                },
            },
            CaptainUpdate::Transcript {
                project,
                member,
                result,
            } => match (Self::panel_mut(view), result) {
                (Some(panel), Ok(data)) => panel.open_thread(project, member, &data),
                (Some(panel), Err(error)) => panel.set_error(Some(error)),
                (None, _) => {}
            },
            CaptainUpdate::Factory {
                project,
                explicit,
                result,
            } => match (Self::panel_mut(view), result) {
                (Some(panel), Ok(data)) => panel.open_factory(project, &data, explicit),
                (Some(panel), Err(error)) => panel.set_error(Some(error)),
                (None, _) => {}
            },
            CaptainUpdate::Workspace { request, result } => {
                self.apply_workspace_update(request, result, view);
            }
        }
    }

    fn load_transcript(&self, project: String, member: String) {
        let args = serde_json::json!({"project": project, "member": member});
        self.spawn_captain(
            vec!["act".into(), "transcript".into()],
            Some(args),
            move |result| CaptainUpdate::Transcript {
                project,
                member,
                result,
            },
        );
    }

    fn load_factory(&self, project: String, name: String, explicit: bool) {
        let args = serde_json::json!({"project": project, "name": name});
        self.spawn_captain(
            vec!["act".into(), "factory-show".into()],
            Some(args),
            move |result| CaptainUpdate::Factory {
                project,
                explicit,
                result,
            },
        );
    }

    /// The `$EDITOR` handoff from a factory review page returned: reload the page.
    pub(crate) fn captain_file_edited(&mut self, view: &AgentView) {
        if let Some(Overlay::Captain(panel)) = &view.overlay {
            if let Some((project, name)) = panel.open_factory_member() {
                self.load_factory(project, name, false);
            }
        }
        self.dirty = true;
    }

    fn open_captain_target(&mut self, target: OpenTarget, view: &mut AgentView) {
        match target {
            OpenTarget::Session {
                path,
                factory,
                draft,
            } => {
                self.captain_parked = None;
                view.overlay = None;
                self.pending_captain_open = Some((PathBuf::from(path), draft, factory));
            }
            OpenTarget::Thread { id, .. } => {
                let selected = Self::panel_mut(view)
                    .and_then(|p| p.selected_project().map(|p| (p.id.clone(), p.lead.clone())));
                let (project, lead) = selected.unwrap_or_default();
                // The lead is addressed as "lead" in Captain actions.
                let member = if id == lead { "lead".to_owned() } else { id };
                self.load_transcript(project, member);
            }
            OpenTarget::Factory => {
                view.overlay = None;
                self.open_factory_page(view);
            }
            OpenTarget::Draft(draft) => {
                // The draft goes above what the chat already holds.
                view.overlay = None;
                let current = view.editor.get_expanded_text();
                if current.trim().is_empty() {
                    view.editor.set_text(&draft);
                } else {
                    view.editor.set_text(&format!("{draft}\n\n{current}"));
                }
            }
        }
        self.dirty = true;
    }

    /// Open a session file in this same view: the draft of the chat being left is stashed
    /// for its return, and the opened chat's own stashed draft comes back.
    pub(crate) async fn open_captain_session(
        &mut self,
        view: &mut AgentView,
        path: PathBuf,
        draft: Option<String>,
        factory: bool,
    ) -> anyhow::Result<()> {
        if self
            .session_file
            .as_deref()
            .is_some_and(|current| same_file(current, &path.to_string_lossy()))
        {
            if factory {
                self.open_factory_page(view);
            }
            if let Some(draft) = draft {
                view.editor.set_text(&draft);
            }
            return Ok(());
        }
        let selection = SessionSelection::Resume(path.clone());
        let id = match create_session(&self.client, &self.create_options(), Some(&selection)).await
        {
            Ok(id) => id,
            Err(error) => {
                let busy = self
                    .captain_context
                    .as_ref()
                    .and_then(|c| {
                        c.chats.iter().find(|chat| {
                            chat.session
                                .as_deref()
                                .is_some_and(|s| same_file(s, &path.to_string_lossy()))
                        })
                    })
                    .is_some_and(|chat| chat.busy);
                let message = if busy {
                    "The Captain is running a turn in that chat. It opens when the turn ends."
                        .to_owned()
                } else {
                    format!("Could not open that chat: {error:#}")
                };
                self.error_row(&message, view);
                return Ok(());
            }
        };
        self.stash_draft_for_switch(view);
        self.attach_session(&id, super::DockFold::FirstFrame)
            .await?;
        self.rebuild_view(view, &super::RebuildKind::Rebind);
        self.restore_prompt_stash_if_editor_empty(view, true);
        if let Some(draft) = draft {
            view.editor.set_text(&draft);
        }
        if factory {
            self.open_factory_page(view);
        }
        // The breadcrumb follows the opened chat.
        if let Some(context) = self.captain_context.as_mut() {
            if let Some(chat) = context.chats.iter().find(|chat| {
                chat.session
                    .as_deref()
                    .is_some_and(|s| same_file(s, &path.to_string_lossy()))
            }) {
                context.chat = chat.label.clone();
            }
        }
        self.sync_captain_header(view);
        self.refresh_captain();
        self.dirty = true;
        Ok(())
    }

    /// The factory page's `n`: the machine picker and its parameters, native in pa.
    pub(crate) fn open_factory_launch(&mut self, view: &mut AgentView) {
        let mut panel = CaptainView::new(view.terminal_rows().saturating_sub(4) as usize);
        let action = panel.start_launch();
        view.overlay = Some(Overlay::Captain(panel));
        self.dirty = true;
        self.run_captain_action(action, view);
    }

    /// `/captain stop|resume|delete`: the panel's own confirm flow on this project's Captain.
    pub(crate) fn captain_command(&mut self, op: &str, view: &mut AgentView) {
        self.captain_pending_key = Some(match op {
            "stop" => "s",
            "resume" => "R",
            _ => "D",
        });
        self.open_captain_panel(view, "");
    }

    /// F7 and `/crew`: pick a chat of this Captain's project.
    pub(crate) fn open_crew_picker(&mut self, view: &mut AgentView) {
        let Some(context) = self.captain_context.clone() else {
            self.note(
                "This chat is not part of a Captain project. Press F6 to open the Captain.",
                view,
            );
            return;
        };
        view.overlay = Some(Overlay::CrewPicker(crate::captain_view::CrewPicker::new(
            context.project_name.clone(),
            context.chat.clone(),
            context.chats,
        )));
        self.refresh_captain();
        self.dirty = true;
    }

    /// F8 and Shift+F8: open the next or previous chat of this Captain project in place.
    pub(crate) fn cycle_crew_chat(&mut self, step: isize, view: &mut AgentView) {
        let Some(context) = self.captain_context.clone() else {
            self.note(
                "This chat is not part of a Captain project. Press F6 to open the Captain.",
                view,
            );
            return;
        };
        let count = context.chats.len() as isize;
        let current = context
            .chats
            .iter()
            .position(|c| c.label == context.chat)
            .unwrap_or(0) as isize;
        let next = (current + step).rem_euclid(count.max(1)) as usize;
        match context.chats.get(next) {
            Some(chat) if next as isize != current => {
                let member = chat.member.clone();
                self.run_captain_action(
                    CaptainAction::Open {
                        project: context.project,
                        member,
                    },
                    view,
                );
            }
            _ => self.note("This project has no other chat.", view),
        }
    }

    /// Shift+F7 and `/captain chat`: back to the Captain's own chat.
    pub(crate) fn open_parent_chat(&mut self, view: &mut AgentView) {
        let Some(context) = &self.captain_context else {
            self.note("This chat is not part of a Captain project.", view);
            return;
        };
        if context.chat == "captain" {
            self.note("This is the Captain chat.", view);
            return;
        }
        let project = context.project.clone();
        self.run_captain_action(
            CaptainAction::Open {
                project,
                member: None,
            },
            view,
        );
    }

    pub(crate) fn handle_crew_picker_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        view: &mut AgentView,
    ) {
        let Some(id) = super::key_event_to_id(&key) else {
            return;
        };
        let chosen = match view.overlay.as_mut() {
            Some(Overlay::CrewPicker(picker)) => picker.handle_key(&id, view.editor.keybindings()),
            _ => return,
        };
        self.dirty = true;
        match chosen {
            crate::captain_view::CrewPick::None => {}
            crate::captain_view::CrewPick::Close => view.overlay = None,
            crate::captain_view::CrewPick::Open(chat) => {
                view.overlay = None;
                let Some(project) = self.captain_context.as_ref().map(|c| c.project.clone()) else {
                    return;
                };
                self.run_captain_action(
                    CaptainAction::Open {
                        project,
                        member: chat.member,
                    },
                    view,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot() -> CaptainSnapshot {
        serde_json::from_value(json!({
            "current_root": "/p",
            "projects": [{
                "id": "p1", "name": "parser", "root": "/p", "lead": "l1",
                "lead_session": "/s/lead.jsonl", "lead_busy": false,
                "crew": [
                    {"id": "c1", "kind": "thread", "title": "parser-fix", "session": "/s/c1.jsonl", "busy": true},
                    {"id": "c2", "kind": "thread", "title": "claude review", "provider": "claude"},
                    {"id": "f1", "kind": "factory", "title": "check · x", "session": "/s/f.jsonl"}
                ]
            }]
        }))
        .unwrap()
    }

    #[test]
    fn a_crew_chat_resolves_its_project_and_breadcrumb() {
        let context = captain_context(&snapshot(), "/s/c1.jsonl").unwrap();
        assert_eq!(context.project, "p1");
        assert_eq!(context.chat, "parser-fix");
        assert_eq!(
            context
                .chats
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["captain", "parser-fix", "claude review"],
            "factory runs are not chats"
        );
        assert_eq!(
            breadcrumb(&context, "F7", "Shift+F7"),
            "captain › parser-fix · F7 switch chat · Shift+F7 captain"
        );
        let lead = captain_context(&snapshot(), "/s/lead.jsonl").unwrap();
        assert_eq!(
            breadcrumb(&lead, "F7", "Shift+F7"),
            "parser › captain · F7 switch chat · Shift+F7 captain"
        );
        assert!(captain_context(&snapshot(), "/s/other.jsonl").is_none());
    }
}
