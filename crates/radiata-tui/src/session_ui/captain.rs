//! The Captain panel's session surface: the panel open (F6, `/captain`), its key loop, the
//! `pa thread captain` calls its actions ride, text steps in pa's prompt editor, in-place
//! switching between a Captain and its crew chats, and the breadcrumb above the prompt.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{AgentView, Overlay};
use crate::captain_view::{
    CaptainAction, CaptainSnapshot, CaptainView, ComposePurpose, OpenTarget,
};
use crate::image_load::LoadedImage;
use crate::interactive::SessionSelection;
use crate::session_ui::picker_viewport_rows;
use crate::session_ui::sessions_fork::create_session;

/// The background status snapshot's run state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptainRefresh {
    Idle,
    Running,
    /// A refresh was requested while one ran: its answer may predate the change.
    RunningStale,
}

/// One background `pa thread captain` call's result.
pub(crate) enum CaptainUpdate {
    Snapshot(Result<Value, String>),
    Acted {
        op: String,
        done: String,
        requested_root: Option<String>,
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
    /// A chart edit's reply; on success the factory's review page loads.
    Saved {
        project: String,
        name: String,
        result: Result<Value, String>,
    },
    /// A Captain file's `edit-history` reply.
    History {
        project: String,
        field: String,
        result: Result<Value, String>,
    },
    /// A workspace page's `pa thread` call.
    Workspace {
        request: crate::workspace_view::WorkspaceRequest,
        result: Result<Value, String>,
    },
}

/// A chat to open in place: its session file, a draft for the editor, and whether the
/// factory page opens over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptainOpen {
    pub path: PathBuf,
    pub draft: Option<String>,
    pub factory: bool,
    /// A repeat of an open that waited for a running turn: its notice was already shown.
    pub retry: bool,
}

/// Host-session address of a Captain-started factory run, resolved from `captain_snapshot`
/// and the daemon roster so the Factory page inspects the run's real host session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedCaptainRun {
    pub project: String,
    pub member: String,
    pub label: String,
    pub run_id: Option<String>,
    pub session_file: Option<String>,
    pub active_session_id: Option<String>,
    pub starting: bool,
}

/// A message for the Captain waiting for the `lead` call before it to answer, so messages
/// reach the Captain in the order they were typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptainLead {
    pub project: String,
    pub text: String,
    pub done: String,
    /// `lead` queues a server-side Captain turn; `steer` reaches a running one.
    pub op: &'static str,
    /// Hub-owned copies of the images attached to the message.
    pub images: Vec<String>,
}

/// The most images one Captain message carries, so a turn's context stays bounded.
const MAX_CAPTAIN_IMAGES: usize = 4;

const STEERED_NOTE: &str = "Steered Captain (delivered to running turn)";
const QUEUED_NOTE: &str = "Queued for Captain — will run when the current turn finishes";

/// `<hub>/captains/attachments`, where Captain chat images live for the turns that read them.
fn captain_attachments_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("PA_CAPTAIN_HOME") {
        return PathBuf::from(home).join("captains/attachments");
    }
    let hub = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| {
            [home.join(".prime/harness"), home.join(".prime/agent")]
                .into_iter()
                .find(|hub| hub.join("captains").is_dir())
        });
    hub.unwrap_or_else(std::env::temp_dir)
        .join("captains/attachments")
}

/// Write each image into `dir` as `<uuid>.<ext>` and return the paths.
fn save_captain_images(dir: &Path, images: &[&LoadedImage]) -> Result<Vec<String>, String> {
    use base64::Engine as _;
    std::fs::create_dir_all(dir).map_err(|e| format!("Could not save the image: {e}"))?;
    images
        .iter()
        .map(|image| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&image.data)
                .map_err(|e| format!("Could not read the image: {e}"))?;
            let ext = image.mime_type.strip_prefix("image/").map_or("png", |ext| {
                if ext == "jpeg" {
                    "jpg"
                } else {
                    ext
                }
            });
            let path = dir.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
            std::fs::write(&path, bytes).map_err(|e| format!("Could not save the image: {e}"))?;
            Ok(path.to_string_lossy().into_owned())
        })
        .collect()
}

/// An `open` reply naming a native session with no handoff, as a live step subagent's is.
fn native_open_target(reply: &Value) -> Option<OpenTarget> {
    if reply.get("kind").and_then(Value::as_str) != Some("native") {
        return None;
    }
    let path = reply.get("session")?.as_str()?.trim();
    (!path.is_empty()).then(|| OpenTarget::Session {
        path: path.to_owned(),
        factory: false,
        draft: None,
    })
}

/// An older backend that knows no `act steer`.
fn steer_unsupported(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("steer")
        && ["unknown", "unsupported", "invalid choice", "not supported"]
            .iter()
            .any(|marker| lower.contains(marker))
}

/// A `steer` or `lead` reply's note: whether the running turn took the message.
fn captain_delivery_note(op: &str, reply: &Value) -> &'static str {
    let steered = reply
        .get("steered")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || reply.get("delivery").and_then(Value::as_str) == Some("steered");
    if op == "steer" && steered {
        STEERED_NOTE
    } else {
        QUEUED_NOTE
    }
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
    pub project_root: String,
    /// The Captain or crew chat the open chat is, or descends from: "captain" or the crew
    /// member's title.
    pub chat: String,
    /// The open subagent's name when the open chat is a subagent of `chat`.
    pub subagent: Option<String>,
    /// Live status of this project's Captain (`working`, `needs_you`, or `idle`).
    pub lead_status: String,
    /// Lead user turns currently waiting in SQLite `jobs(status='queued')`.
    pub lead_queued: Vec<String>,
    /// Every chat of the project that radiata can open: the Captain, the crew, and live runs.
    pub chats: Vec<CrewChat>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CrewChat {
    pub label: String,
    pub session: Option<String>,
    pub member: Option<String>,
    pub busy: bool,
    /// needs_you, working, paused, failed, or idle.
    pub status: String,
    /// The short model badge.
    pub model: Option<String>,
    /// captain, chat, factory, or the provider of a provider CLI thread.
    pub kind: String,
    /// A Captain run's id in its host session.
    pub run_id: Option<String>,
}

impl CrewChat {
    /// A provider CLI thread: no native session, so the panel's thread view shows it.
    pub(crate) fn is_provider_thread(&self) -> bool {
        self.session.is_none() && self.member.is_some() && self.kind != "factory"
    }
}

fn same_file(a: &str, b: &str) -> bool {
    let canonical = |p: &str| {
        Path::new(p)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(p))
    };
    a == b || canonical(a) == canonical(b)
}

/// The switchable chats of one project: the Captain, its crew chats and provider threads,
/// and its live factory runs.
fn project_chats(project: &crate::captain_view::CaptainProject) -> Vec<CrewChat> {
    let lead_status = match project.lead_state.as_str() {
        "needs_you" => "needs_you",
        _ if project.lead_busy || project.lead_state == "running" => "working",
        _ => "idle",
    };
    let short = |model: &str| {
        (!model.is_empty()).then(|| model.rsplit('/').next().unwrap_or(model).to_owned())
    };
    let mut chats = vec![CrewChat {
        label: "captain".into(),
        session: project.lead_session.clone(),
        member: None,
        busy: project.lead_busy,
        status: lead_status.into(),
        model: short(&project.model),
        kind: "captain".into(),
        run_id: None,
    }];
    let live_run = |m: &crate::captain_view::CaptainMember| {
        !matches!(
            m.run_state.as_deref(),
            Some("done" | "failed" | "stopped" | "lost" | "cancelled")
        ) && m.state != "done"
    };
    chats.extend(
        project
            .crew
            .iter()
            .filter(|m| !m.archived && (!m.is_run() || live_run(m)))
            .map(|m| CrewChat {
                label: m.label(),
                session: (!m.is_run()).then(|| m.session.clone()).flatten(),
                member: Some(m.id.clone()),
                busy: m.busy,
                status: m.chat_status().into(),
                model: (!m.is_run()).then(|| m.short_model_badge()).flatten(),
                kind: if m.is_run() {
                    "factory".into()
                } else if m.session.is_some() {
                    "chat".into()
                } else {
                    m.provider.clone()
                },
                run_id: m.run_id.clone(),
            }),
    );
    chats
}

/// The project and chat label for `session_file`, from a status snapshot. `ancestors` are
/// the session files `session_file` descends from (a subagent's parent first), so a subagent
/// of a Captain or crew chat keeps its project.
pub(crate) fn captain_context(
    snapshot: &CaptainSnapshot,
    session_file: &str,
    ancestors: &[(String, String)],
) -> Option<CaptainContext> {
    let lookup = std::iter::once((session_file, None)).chain(
        ancestors
            .iter()
            .map(|(file, name)| (file.as_str(), Some(name))),
    );
    for (file, _) in lookup.clone() {
        for project in &snapshot.projects {
            let chats = project_chats(project);
            let current = chats.iter().find(|chat| {
                chat.session
                    .as_deref()
                    .is_some_and(|path| same_file(path, file))
            });
            if let Some(current) = current {
                let subagent = (file != session_file).then(|| {
                    lookup
                        .clone()
                        .find_map(|(_, name)| name.cloned())
                        .unwrap_or_else(|| "subagent".into())
                });
                let lead_status = chats
                    .first()
                    .map(|c| c.status.clone())
                    .unwrap_or_else(|| "idle".into());
                return Some(CaptainContext {
                    project: project.id.clone(),
                    project_name: project.name.clone(),
                    project_root: project.root.clone(),
                    chat: current.label.clone(),
                    subagent,
                    lead_status,
                    lead_queued: project.lead_queued.clone(),
                    chats,
                });
            }
        }
    }
    None
}

/// The breadcrumb above the prompt in a Captain chat. The Captain's own chat names no
/// return-to-Captain key; a crew chat and a subagent do.
pub(crate) fn breadcrumb(
    context: &CaptainContext,
    switch_key: &str,
    parent_key: &str,
    overview_key: &str,
) -> String {
    let mut path = format!("{} › {}", context.project_name, context.chat);
    if let Some(subagent) = &context.subagent {
        path.push_str(" › ");
        path.push_str(subagent);
    }
    if context.chat == "captain" && context.subagent.is_none() {
        let queued = context.lead_queued.len();
        let status = match (context.lead_status.as_str(), queued) {
            ("working", n) if n > 0 => Some(format!("running ({n} queued)")),
            ("working", _) => Some("running".to_string()),
            ("needs_you", n) if n > 0 => Some(format!("needs you ({n} queued)")),
            ("needs_you", _) => Some("needs you".to_string()),
            (_, n) if n > 0 => Some(format!("{n} queued")),
            _ => None,
        };
        match status {
            Some(status) => {
                format!("{path} · {status} · {switch_key} switch · {overview_key} overview")
            }
            None => format!("{path} · {switch_key} switch · {overview_key} overview"),
        }
    } else {
        format!("{path} · {switch_key} switch · {parent_key} captain · {overview_key} overview")
    }
}

/// The session files `session_file` descends from in the daemon roster, nearest first, each
/// with the name of the session below it (the subagent the breadcrumb shows).
pub(crate) fn session_ancestors(roster: &[Value], session_file: &str) -> Vec<(String, String)> {
    let summary_of = |file: &str| {
        roster
            .iter()
            .filter_map(|entry| entry.get("summary"))
            .find(|summary| {
                summary
                    .get("sessionFile")
                    .and_then(Value::as_str)
                    .is_some_and(|path| same_file(path, file))
            })
    };
    let mut out = Vec::new();
    let mut file = session_file.to_owned();
    while out.len() < 16 {
        let Some(summary) = summary_of(&file) else {
            break;
        };
        let text = |key: &str| {
            summary
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let own = text("sessionName").or_else(|| text("name"));
        let Some(parent) = text("parentSessionPath") else {
            break;
        };
        out.push((parent.clone(), own.unwrap_or_else(|| "subagent".into())));
        file = parent;
    }
    out
}

impl super::SessionUi {
    /// The Captain context of `file`, through its roster ancestors for a subagent.
    pub(super) fn context_for(&self, snapshot: &CaptainSnapshot, file: &str) -> Option<CaptainContext> {
        captain_context(snapshot, file, &session_ancestors(&self.roster, file))
    }

    /// Subagents per crew session, from the daemon roster, for the panel's `◆N` badges.
    fn crew_subagents(&self) -> std::collections::HashMap<String, usize> {
        let mut counts = std::collections::HashMap::new();
        for project in self.captain_snapshot.iter().flat_map(|s| &s.projects) {
            for session in project.crew.iter().filter_map(|m| m.session.as_ref()) {
                let identity =
                    crate::subagents::SessionIdentity::new(None, None, Some(session.clone()));
                let total = crate::subagents::count_descendants(&self.roster, &identity).total;
                if total > 0 {
                    counts.insert(session.clone(), total);
                }
            }
        }
        counts
    }

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
        let socket = self.daemon_socket.clone();
        tokio::spawn(async move {
            let result = async {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let mut command = tokio::process::Command::new(exe);
                command
                    .arg("thread")
                    .args(&args)
                    .current_dir(&cwd)
                    .env_remove("PA_CAPTAIN_ACTOR")
                    // Captain turns run on this chat's daemon, so the chat can show them live.
                    .env("PA_CAPTAIN_NATIVE_SOCKET", &socket)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);
                if let Some(session) = &session {
                    command.env("PA_CAPTAIN_SESSION", session);
                }
                let mut child = command
                    .spawn()
                    .map_err(|e| format!("Could not start radiata: {e}"))?;
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
        let requested_root = args
            .get("root")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        self.spawn_captain(vec!["act".into(), op.into()], Some(args), move |result| {
            CaptainUpdate::Acted {
                op: op_name,
                done,
                requested_root,
                result,
            }
        });
    }

    /// Start a status snapshot. While one runs, a new request only marks it stale, so a slow
    /// status under load never stacks concurrent Python processes; the landing snapshot then
    /// starts exactly one follow-up run.
    pub(crate) fn refresh_captain(&mut self) {
        self.captain_refresh = match self.captain_refresh {
            CaptainRefresh::Idle => {
                self.spawn_captain(vec!["status".into()], None, CaptainUpdate::Snapshot);
                CaptainRefresh::Running
            }
            CaptainRefresh::Running | CaptainRefresh::RunningStale => CaptainRefresh::RunningStale,
        };
    }

    /// Cheap `stat`-only poll of `<hub>/captains/status/<project-id>.json`.
    /// Never starts Python or `git`: updates the bottom activity dock and emits
    /// crew/factory change toasts directly from the status file.
    pub(crate) fn poll_captain_status(&mut self, view: &mut AgentView) {
        use crate::interactive::captain_hint::{status_toasts, CaptainStatusPoll};
        match self.captain_status_watcher.poll() {
            CaptainStatusPoll::Unchanged => {}
            CaptainStatusPoll::Changed(Some(status)) => {
                let (states, toasts) = status_toasts(self.captain_states.as_ref(), &status);
                self.captain_states = Some(states);
                for toast in toasts {
                    view.toasts.push(toast);
                    self.dirty = true;
                }
                self.captain_dock = Some(status.to_dock_state());
                self.sync_activity_dock(view);
                // A status change may mean the turn that held a chat ended.
                retry_busy_open(&mut self.pending_captain_open, &mut self.captain_busy_open);
            }
            CaptainStatusPoll::Changed(None) => {
                if self.captain_dock.take().is_some() {
                    self.sync_activity_dock(view);
                }
            }
        }
        // When a chat open is waiting for a turn to end (possibly in another project's folder),
        // or when an optimistic Captain lead submit is still waiting without a live daemon turn,
        // check the hub DB stamp so we retry/settle even while the Captain overlay is closed.
        let waiting_lead = self.optimistic_user_echo.is_some()
            && !self.turn_active
            && self.captain_lead_inflight.is_none()
            && self
                .captain_context
                .as_ref()
                .is_some_and(|c| c.chat == "captain" && c.subagent.is_none());
        if self.captain_busy_open.is_some() || waiting_lead {
            if self.captain_busy_open.is_some() {
                retry_busy_open(&mut self.pending_captain_open, &mut self.captain_busy_open);
            }
            let stamp = self.captain_status_watcher.db_stamp();
            if waiting_lead || stamp.is_none() || stamp != self.captain_db_stamp {
                self.captain_db_stamp = stamp;
                self.refresh_captain();
            }
        }
    }

    pub(crate) fn captain_hub_present(&self) -> bool {
        self.captain_status_watcher.hub_present()
    }

    /// The open panel's poll: skips spawning Python when the hub DB and status
    /// file have not changed since the last snapshot.
    pub(crate) fn poll_captain(&mut self, view: &AgentView) {
        let mut need_snapshot = false;
        if let Some(Overlay::Captain(panel)) = &view.overlay {
            if let Some((project, member)) = panel.open_thread_member() {
                self.load_transcript(project, member);
            }
            if let Some((project, name)) = panel.open_factory_member() {
                self.load_factory(project, name, false);
            }
        }
        let stamp = self.captain_status_watcher.db_stamp();
        if stamp.is_none() || stamp != self.captain_db_stamp {
            self.captain_db_stamp = stamp;
            need_snapshot = true;
        }
        if self.captain_busy_open.is_some() {
            retry_busy_open(&mut self.pending_captain_open, &mut self.captain_busy_open);
            need_snapshot = true;
        }
        if self.optimistic_user_echo.is_some()
            && !self.turn_active
            && self.captain_lead_inflight.is_none()
            && self
                .captain_context
                .as_ref()
                .is_some_and(|c| c.chat == "captain" && c.subagent.is_none())
        {
            need_snapshot = true;
        }
        if need_snapshot {
            self.refresh_captain();
        }
    }

    /// The startup hint when this project has no Captain, from the watcher's resolved root.
    pub(crate) fn missing_captain_hint(&self) -> Option<&'static str> {
        self.captain_status_watcher.missing_captain_hint()
    }

    /// Read the Captain records when this chat may belong to a project, for the breadcrumb.
    pub(crate) fn refresh_captain_context(&mut self) {
        if self.session_file.is_some() && self.captain_status_watcher.has_captain() {
            self.refresh_captain();
        }
    }

    /// F6 and `/captain [name|new|promote]`: open the native Captain panel.
    pub(crate) fn open_captain_panel(&mut self, view: &mut AgentView, query: &str) {
        // A Captain text step still holding the editor ends here: its panel parks (a setup
        // keeps its answers as a draft) and the chat's draft comes back.
        let composing = matches!(
            self.captain_compose,
            Some(CaptainCompose {
                owner: ComposeOwner::Captain { .. },
                ..
            })
        );
        let mut composed = None;
        if composing {
            if let Some(compose) = self.captain_compose.take() {
                let typed = view.editor.get_expanded_text();
                view.editor.set_text(&compose.saved_draft);
                if let ComposeOwner::Captain {
                    purpose,
                    panel: Some(mut panel),
                } = compose.owner
                {
                    if purpose == ComposePurpose::Brief {
                        panel.keep_brief_text(&typed);
                    }
                    panel.park();
                    composed = Some(panel);
                }
                self.sync_captain_header(view);
            }
        }
        let mut panel = composed
            .or_else(|| self.captain_parked.take())
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

    /// The Captain panel on screen, or dimmed behind a floating model picker.
    fn panel_mut(view: &mut AgentView) -> Option<&mut CaptainView> {
        match view.overlay.as_mut() {
            Some(Overlay::Captain(panel)) => Some(panel),
            _ => match view.modal_backdrop.as_deref_mut() {
                Some(Overlay::Captain(panel)) => Some(panel),
                _ => None,
            },
        }
    }

    pub(crate) fn run_captain_action(&mut self, action: CaptainAction, view: &mut AgentView) {
        match action {
            CaptainAction::None => {}
            CaptainAction::Close => {
                // The panel parks: F6 reopens it on the same project, row, tab, and scroll.
                if let Some(Overlay::Captain(mut panel)) = view.overlay.take() {
                    if !panel.is_transient() {
                        panel.park();
                        self.captain_parked = Some(panel);
                    }
                }
                self.close_overlay_to_origin(view);
            }
            CaptainAction::OpenFactoryRun {
                project,
                member,
                run_id,
            } => self.open_captain_run(view, project, member, run_id),
            CaptainAction::LoadHistory { project, field } => {
                if let Some(panel) = Self::panel_mut(view) {
                    panel.set_notice("Loading history…");
                }
                let args = serde_json::json!({"project": project, "field": field});
                self.spawn_captain(
                    vec!["act".into(), "edit-history".into()],
                    Some(args),
                    move |result| CaptainUpdate::History {
                        project,
                        field,
                        result,
                    },
                );
            }
            CaptainAction::OpenSubagents { session } => self.open_crew_subagents(view, &session),
            CaptainAction::Refresh => self.refresh_captain(),
            CaptainAction::Act { op, args, done } => {
                if let Some(panel) = Self::panel_mut(view) {
                    panel.set_notice("Working…");
                }
                self.captain_act(&op, args, done);
            }
            CaptainAction::SaveFactory {
                op,
                project,
                name,
                args,
            } => {
                self.spawn_captain(vec!["act".into(), op], Some(args), move |result| {
                    CaptainUpdate::Saved {
                        project,
                        name,
                        result,
                    }
                });
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
                // The review page replaces the panel; Esc returns to the panel.
                if let Some(Overlay::Captain(panel)) = view.overlay.take() {
                    self.captain_parked = Some(panel);
                }
                self.open_workspace(
                    crate::workspace_view::WorkspaceView::review(Some((project, member))),
                    view,
                );
            }
            CaptainAction::PickModel(target) => {
                if let Some(Overlay::Captain(panel)) = view.overlay.take() {
                    self.captain_parked = Some(panel);
                }
                let (current_model, current_thinking) = match &target {
                    crate::captain_view::CaptainModelTarget::Setup {
                        current_model,
                        current_thinking,
                    }
                    | crate::captain_view::CaptainModelTarget::CaptainLead {
                        current_model,
                        current_thinking,
                        ..
                    }
                    | crate::captain_view::CaptainModelTarget::Crew {
                        current_model,
                        current_thinking,
                        ..
                    }
                    | crate::captain_view::CaptainModelTarget::FactoryNode {
                        current_model,
                        current_thinking,
                        ..
                    }
                    | crate::captain_view::CaptainModelTarget::FactoryDefault {
                        current_model,
                        current_thinking,
                        ..
                    }
                    | crate::captain_view::CaptainModelTarget::SetupFactory {
                        current_model,
                        current_thinking,
                    } => (current_model.clone(), current_thinking.clone()),
                    crate::captain_view::CaptainModelTarget::Assign { .. } => {
                        (String::new(), String::new())
                    }
                };
                self.open_shared_model_picker(
                    view,
                    &current_model,
                    &current_thinking,
                    super::model_picker::ModelPickTarget::Captain(target),
                );
            }
        }
    }

    /// Apply a model + thinking choice returned from pa's shared `ModelPicker` to a Captain flow.
    pub(crate) fn apply_captain_picked_model(
        &mut self,
        target: crate::captain_view::CaptainModelTarget,
        full_model: String,
        effort_edited: bool,
        thinking: String,
        view: &mut AgentView,
    ) {
        use crate::captain_view::CaptainModelTarget;
        match target {
            CaptainModelTarget::Setup { .. } => {
                if let Some(panel) = Self::panel_mut(view) {
                    let action = panel.apply_setup_model(full_model, thinking);
                    self.run_captain_action(action, view);
                }
            }
            CaptainModelTarget::SetupFactory { .. } => {
                if let Some(panel) = Self::panel_mut(view) {
                    let action = panel.apply_setup_factory_model(full_model, thinking);
                    self.run_captain_action(action, view);
                }
            }
            CaptainModelTarget::FactoryDefault { project, name, .. } => {
                let args = serde_json::json!({
                    "project": project,
                    "name": name,
                    "model": full_model,
                    "thinking": thinking,
                });
                if let Some(panel) = Self::panel_mut(view) {
                    panel.set_notice("Updating the default model…");
                }
                self.spawn_captain(
                    vec!["act".into(), "factory-set-default-model".into()],
                    Some(args),
                    move |result| CaptainUpdate::Factory {
                        project,
                        explicit: true,
                        result,
                    },
                );
            }
            CaptainModelTarget::CaptainLead { project, .. } => {
                let args = captain_lead_model_args(project, full_model, effort_edited, thinking);
                if let Some(panel) = Self::panel_mut(view) {
                    panel.set_notice("Updating model…");
                }
                self.captain_act("edit", args, "Captain model updated.".into());
            }
            CaptainModelTarget::Assign { project } => {
                if let Some(panel) = Self::panel_mut(view) {
                    panel.apply_assign_model(project, full_model, thinking);
                }
            }
            CaptainModelTarget::Crew { member, .. }
                if member.starts_with(crate::captain_view::COMPOSE_MEMBER) =>
            {
                let stage = &member[crate::captain_view::COMPOSE_MEMBER.len()..];
                if let Some(panel) = Self::panel_mut(view) {
                    panel.apply_compose_model(stage, full_model, thinking);
                }
            }
            CaptainModelTarget::Crew { member, .. }
                if member.starts_with(crate::captain_view::SPECIALIST_MEMBER) =>
            {
                let id = &member[crate::captain_view::SPECIALIST_MEMBER.len()..];
                if let Some(panel) = Self::panel_mut(view) {
                    let action =
                        panel.apply_specialist_model(id, full_model, effort_edited, thinking);
                    self.run_captain_action(action, view);
                }
            }
            CaptainModelTarget::Crew {
                project,
                member,
                label,
                ..
            } => {
                let mut args = serde_json::json!({
                    "project": project,
                    "member": member,
                    "model": full_model,
                });
                if effort_edited {
                    args["thinking"] = Value::String(thinking);
                }
                if let Some(panel) = Self::panel_mut(view) {
                    panel.set_notice(format!("Updating model for {label}…"));
                }
                self.captain_act("crew-model", args, format!("Updated model for {label}."));
            }
            CaptainModelTarget::FactoryNode {
                project,
                name,
                state_id,
                role,
                current_thinking,
                ..
            } => {
                let effective_thinking = if effort_edited {
                    thinking
                } else {
                    current_thinking
                };
                let mut args = serde_json::json!({
                    "project": project,
                    "name": name,
                    "state": state_id,
                    "model": full_model,
                    "thinking": effective_thinking,
                });
                if role.is_some() {
                    args["edit_role"] = Value::Bool(true);
                }
                if let Some(panel) = Self::panel_mut(view) {
                    panel.set_notice(format!("Updating model for {state_id}…"));
                }
                self.spawn_captain(
                    vec!["act".into(), "factory-set-model".into()],
                    Some(args),
                    move |result| CaptainUpdate::Factory {
                        project,
                        explicit: true,
                        result,
                    },
                );
            }
        }
    }

    /// A message typed in the Captain's own chat becomes a Captain turn (a `lead` job), so the
    /// Captain reads its rules, plan, crew reports, and operations, and the backend counts it as
    /// a user turn for approvals and grants. The job runs through this chat's daemon, so the
    /// message and reply appear here. Returns true when the message was taken.
    ///
    /// A busy Captain is steered (`act steer`, which the backend turns into a queued `lead`
    /// job when the running turn cannot take it), so the backend holds the message, not pa.
    pub(crate) fn send_to_captain(
        &mut self,
        text: &str,
        behavior: super::prompt::SubmitBehavior,
        view: &mut AgentView,
    ) -> bool {
        let Some((project, busy)) = self
            .captain_context
            .as_ref()
            .filter(|context| context.chat == "captain" && context.subagent.is_none())
            .map(|context| {
                let busy = context
                    .chats
                    .iter()
                    .any(|chat| chat.label == "captain" && chat.busy);
                (context.project.clone(), busy)
            })
        else {
            return false;
        };
        let attached: Vec<&LoadedImage> =
            crate::image_markers::collect_marked_images(&self.pasted_images, text)
                .into_iter()
                .map(|(_, image)| image)
                .collect();
        if attached.len() > MAX_CAPTAIN_IMAGES {
            view.editor.set_text(text);
            self.note(
                &format!(
                    "The Captain takes up to {MAX_CAPTAIN_IMAGES} images per message. Remove {} to send.",
                    attached.len() - MAX_CAPTAIN_IMAGES
                ),
                view,
            );
            return true;
        }
        let images = match save_captain_images(&captain_attachments_dir(), &attached) {
            Ok(images) => images,
            Err(error) => {
                view.editor.set_text(text);
                self.note(&error, view);
                return true;
            }
        };
        view.editor.add_to_history(text);
        let running = busy || self.turn_active || self.captain_lead_inflight.is_some();
        let (op, done) = if running {
            match behavior {
                super::prompt::SubmitBehavior::FollowUp => ("lead", QUEUED_NOTE.to_owned()),
                super::prompt::SubmitBehavior::Steer => ("steer", STEERED_NOTE.to_owned()),
            }
        } else {
            ("lead", String::new())
        };
        if !running {
            self.show_optimistic_submit(text, view);
            self.captain_lead_sent_at = Some(std::time::Instant::now());
        }
        self.captain_lead_queue.push_back(CaptainLead {
            project,
            text: text.to_owned(),
            done,
            op,
            images,
        });
        self.send_next_captain_lead();
        true
    }

    /// Start the next queued `lead` call once the one before it answered.
    fn send_next_captain_lead(&mut self) {
        if self.captain_lead_inflight.is_some() {
            return;
        }
        let Some(lead) = self.captain_lead_queue.pop_front() else {
            return;
        };
        self.captain_act(lead.op, captain_lead_args(&lead), lead.done.clone());
        self.captain_lead_inflight = Some(lead);
    }

    /// A `lead` or `steer` call answered. A `steer` an older backend refuses is sent again as
    /// a `lead` job. A refused message goes back above the editor's text, and the messages
    /// typed after it stay in the editor too, so none is sent out of order.
    fn captain_lead_answered(&mut self, error: Option<&str>, view: &mut AgentView) {
        let Some(mut lead) = self.captain_lead_inflight.take() else {
            return;
        };
        if lead.op == "steer" && error.is_some_and(steer_unsupported) {
            lead.op = "lead";
            lead.done = QUEUED_NOTE.to_owned();
            self.captain_lead_queue.push_front(lead);
            self.send_next_captain_lead();
            return;
        }
        if error.is_some() {
            let same_project = self
                .captain_context
                .as_ref()
                .is_some_and(|c| c.project == lead.project && c.chat == "captain");
            let mut texts = vec![lead.text];
            // Only drain queued messages that belong to the same failed project; leave
            // messages queued for other projects intact.
            let mut remaining = std::collections::VecDeque::new();
            while let Some(queued) = self.captain_lead_queue.pop_front() {
                if queued.project == lead.project {
                    texts.push(queued.text);
                } else {
                    remaining.push_back(queued);
                }
            }
            self.captain_lead_queue = remaining;
            if same_project {
                self.captain_lead_sent_at = None;
                self.rollback_optimistic_submit(view);
                prepend_draft(view, &texts.join("\n\n"));
            } else {
                view.toasts.push(format!(
                    "Captain message failed: {}",
                    error.unwrap_or("unknown error")
                ));
            }
            self.send_next_captain_lead();
            return;
        }
        if self.optimistic_user_echo.is_some() && !self.turn_active {
            self.captain_lead_sent_at = Some(std::time::Instant::now());
        }
        self.refresh_captain();
        self.send_next_captain_lead();
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
        // Read before the chat's draft comes back: a cancelled brief keeps what was typed.
        let typed = view.editor.get_expanded_text();
        let text = submit.then(|| typed.clone());
        view.editor.set_text(&compose.saved_draft);
        self.dirty = true;
        match compose.owner {
            ComposeOwner::Captain { purpose, panel } => {
                let mut panel = panel.unwrap_or_else(|| {
                    CaptainView::new(view.terminal_rows().saturating_sub(4) as usize)
                });
                if cancel && purpose == ComposePurpose::Brief {
                    panel.keep_brief_text(&typed);
                }
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

    /// Keep the bottom status watcher and queued-message strip aligned with the
    /// currently open Captain project (never stuck on the terminal's launch folder).
    fn sync_captain_active_project(&mut self, view: &mut AgentView) {
        if let Some(context) = &self.captain_context {
            if !context.project_root.is_empty() {
                self.captain_status_watcher
                    .set_project_root(PathBuf::from(&context.project_root));
            }
            if context.chat == "captain" && context.subagent.is_none() {
                if !context.lead_queued.is_empty() {
                    view.queued.follow_ups = context.lead_queued.clone();
                    self.captain_queue_mirrored = true;
                } else if self.captain_queue_mirrored {
                    view.queued.follow_ups.clear();
                    self.captain_queue_mirrored = false;
                }
                return;
            }
        }
        if self.captain_queue_mirrored {
            view.queued.follow_ups.clear();
            self.captain_queue_mirrored = false;
        }
    }

    /// The line above the prompt: the text step's label, else the Captain breadcrumb.
    pub(crate) fn sync_captain_header(&mut self, view: &mut AgentView) {
        let submit = Self::key_text(view, "tui.input.submit");
        view.blank_compose_canvas = match &self.captain_compose {
            Some(CaptainCompose {
                owner:
                    ComposeOwner::Captain {
                        purpose: ComposePurpose::Brief,
                        ..
                    },
                label,
                ..
            }) => Some(format!(
                "{label} — describe what this project should achieve below"
            )),
            _ => None,
        };
        view.editor_header = if let Some(compose) = &self.captain_compose {
            Some(format!("{} · {submit} done · Esc cancel", compose.label))
        } else {
            self.captain_context.as_ref().map(|context| {
                breadcrumb(
                    context,
                    &Self::key_text(view, "app.captain.crew"),
                    &Self::key_text(view, "app.captain.parent"),
                    &Self::key_text(view, "app.factory.toggle"),
                )
            })
        };
    }

    pub(crate) fn apply_captain_update(&mut self, update: CaptainUpdate, view: &mut AgentView) {
        self.dirty = true;
        match update {
            CaptainUpdate::Snapshot(result) => {
                let stale = self.captain_refresh == CaptainRefresh::RunningStale;
                self.captain_refresh = CaptainRefresh::Idle;
                if stale {
                    self.refresh_captain();
                }
                self.captain_db_stamp = self.captain_status_watcher.db_stamp();
                if let Ok(data) = &result {
                    if let Ok(snapshot) = serde_json::from_value::<CaptainSnapshot>(data.clone()) {
        self.captain_context = self
            .session_file
            .clone()
            .and_then(|file| self.context_for(&snapshot, &file));
        self.sync_captain_active_project(view);
        self.sync_captain_header(view);
                        let (states, toasts) = crate::captain_view::crew_toasts(
                            self.captain_states.as_ref(),
                            &snapshot,
                        );
                        self.captain_states = Some(states);
                        for toast in toasts {
                            view.toasts.push(toast);
                        }
                        self.captain_snapshot = Some(snapshot);
                        self.update_subagent_summary(view);
                        // If an optimistic Captain lead submit never reached this daemon (e.g.
                        // the Captain job failed immediately with `needs_you` or settled elsewhere),
                        // clear the stuck Waiting loader and show why instead of spinning forever.
                        if self.optimistic_user_echo.is_some()
                            && !self.turn_active
                            && self.captain_lead_inflight.is_none()
                        {
                            if let Some(project) = self
                                .captain_context
                                .as_ref()
                                .filter(|c| c.chat == "captain" && c.subagent.is_none())
                                .and_then(|c| {
                                    self.captain_snapshot
                                        .as_ref()
                                        .and_then(|s| s.projects.iter().find(|p| p.id == c.project))
                                })
                                .cloned()
                            {
                                if !project.lead_busy && project.lead_state != "running" {
                                    if project.lead_state == "needs_you" {
                                        self.captain_lead_sent_at = None;
                                        self.rollback_optimistic_submit(view);
                                        let detail = project
                                            .lead_detail
                                            .as_deref()
                                            .map(str::trim)
                                            .filter(|s| !s.is_empty())
                                            .unwrap_or("Check F6 for details.");
                                        self.error_row(
                                            &format!("Captain could not start: {detail}"),
                                            view,
                                        );
                                    } else if self.captain_lead_sent_at.is_some_and(|sent| {
                                        sent.elapsed() >= std::time::Duration::from_secs(3)
                                    }) {
                                        self.captain_lead_sent_at = None;
                                        self.rollback_optimistic_submit(view);
                                        self.note(
                                            "Captain is idle (no turn started). Press F6 to inspect or resend.",
                                            view,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                self.poll_captain_status(view);
                retry_busy_open(&mut self.pending_captain_open, &mut self.captain_busy_open);
                let subagents = self.crew_subagents();
                if let Some(panel) = Self::panel_mut(view) {
                    panel.set_subagents(subagents);
                    match result {
                        Ok(data) => {
                            panel.apply_snapshot(&data);
                            if let Some(query) = self.captain_focus_query.take() {
                                if !panel.focus_project(&query) {
                                    if stale || self.captain_refresh != CaptainRefresh::Idle {
                                        // A newer snapshot is already in flight (e.g. post-init
                                        // refresh); keep the query for it instead of flashing an
                                        // error on this older snapshot.
                                        self.captain_focus_query = Some(query);
                                    } else {
                                        panel.set_notice(format!("No project matches {query}."));
                                    }
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
                        if let Some(project) = self
                            .captain_snapshot
                            .as_ref()
                            .and_then(|s| s.projects.iter().find(|p| p.id == context.project))
                        {
                            picker.set_project(project);
                        }
                    }
                }
            }
            CaptainUpdate::Acted {
                op,
                done,
                requested_root,
                result,
            } => {
                if matches!(op.as_str(), "lead" | "steer") {
                    let error = result.as_ref().err().map(String::as_str);
                    let resent = op == "steer" && error.is_some_and(steer_unsupported);
                    self.captain_lead_answered(error, view);
                    if resent {
                        return;
                    }
                }
                let done = match (&result, done.is_empty()) {
                    (Ok(reply), false) if matches!(op.as_str(), "lead" | "steer") => {
                        captain_delivery_note(&op, reply).to_owned()
                    }
                    _ => done,
                };
                match result {
                    Ok(reply) => {
                        let target = if op == "new-thread" {
                            OpenTarget::from_new_thread(&reply)
                        } else {
                            OpenTarget::from_reply(&reply)
                        };
                        if let Some(target) = target {
                            self.open_captain_target(target, view);
                            return;
                        }
                        if matches!(op.as_str(), "init" | "promote") {
                            let created_id = reply
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned);
                            let created_root = reply
                                .get("root")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned);
                            // Clear unfinished setup drafts (for cwd, requested root, and canonical created root),
                            // even if the panel was parked or closed while `init` was running.
                            if let Some(panel) =
                                Self::panel_mut(view).or(self.captain_parked.as_mut())
                            {
                                panel.clear_draft();
                                if let Some(root) = requested_root.as_deref() {
                                    panel.clear_draft_for(root);
                                }
                                if let Some(root) = created_root.as_deref() {
                                    panel.clear_draft_for(root);
                                }
                            } else {
                                if let Some(root) = requested_root.as_deref() {
                                    crate::captain_view::clear_saved_setup_draft(root);
                                }
                                if let Some(root) = created_root.as_deref() {
                                    crate::captain_view::clear_saved_setup_draft(root);
                                }
                            }
                            // Select the newly created project by id (falling back to root or cwd).
                            self.captain_focus_query = created_id.or(created_root).or_else(|| {
                                Self::panel_mut(view)
                                    .map(|panel| panel.snapshot().current_root.clone())
                            });
                            if op == "init" {
                                if let Some(path) = reply
                                    .get("lead_session")
                                    .or_else(|| reply.get("session"))
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                {
                                    self.open_captain_target(
                                        OpenTarget::Session {
                                            path: path.to_owned(),
                                            factory: false,
                                            draft: None,
                                        },
                                        view,
                                    );
                                    self.refresh_captain();
                                    return;
                                }
                            }
                        }
                        let mut notice = done;
                        if op == "stop" && reply["state"] == "stopping" {
                            notice = "Stopping. Running work is ending.".into();
                        }
                        if op == "delete" && reply["state"] != "deleted" {
                            notice = "Deleting. Running work is ending.".into();
                        }
                        if op == "archive-project" && reply["state"] == "archiving" {
                            notice = "Archiving. Running work is ending.".into();
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
                }
            }
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
                Ok(reply) => {
                    match OpenTarget::from_reply(&reply).or_else(|| native_open_target(&reply)) {
                        Some(target) => self.open_captain_target(target, view),
                        None => {
                            if let Some(panel) = Self::panel_mut(view) {
                                panel.set_error(Some("Captain returned nothing to open.".into()));
                            }
                        }
                    }
                }
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
            } => {
                if let Some(panel) = Self::panel_mut(view) {
                    match result {
                        Ok(data) => {
                            let notice = data
                                .get("notice")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                            panel.open_factory(project, &data, explicit);
                            if let Some(notice) = notice {
                                panel.set_notice(notice);
                            }
                            self.poll_captain_status(view);
                        }
                        Err(error) => panel.set_error(Some(error)),
                    }
                }
            }
            CaptainUpdate::Saved {
                project,
                name,
                result,
            } => match result {
                Ok(reply) => {
                    if let Some(panel) = Self::panel_mut(view) {
                        let notice = reply["notice"].as_str().unwrap_or("Saved.").to_owned();
                        panel.set_notice(notice);
                    }
                    self.refresh_captain();
                    self.load_factory(project, name, true);
                }
                Err(error) => match Self::panel_mut(view) {
                    Some(panel) => panel.compose_failed(error),
                    None => self.error_row(&error, view),
                },
            },
            CaptainUpdate::History {
                project,
                field,
                result,
            } => match (Self::panel_mut(view), result) {
                (Some(panel), Ok(data)) => panel.open_history(project, field, &data),
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

    pub(super) fn open_captain_target(&mut self, target: OpenTarget, view: &mut AgentView) {
        match target {
            OpenTarget::Session {
                path,
                factory,
                draft,
            } => {
                if let Some(Overlay::Captain(mut panel)) = view.overlay.take() {
                    if !panel.is_transient() {
                        panel.park();
                        self.captain_parked = Some(panel);
                    }
                }
                // A chat the user picks replaces one still waiting for a turn to end.
                self.captain_busy_open = None;
                self.pending_captain_open = Some(CaptainOpen {
                    path: PathBuf::from(path),
                    draft,
                    factory,
                    retry: false,
                });
            }
            OpenTarget::Thread { id, title } => {
                let selected = Self::panel_mut(view)
                    .and_then(|p| p.selected_project().map(|p| (p.id.clone(), p.lead.clone())));
                match selected {
                    Some((project, lead)) => {
                        // The lead is addressed as "lead" in Captain actions.
                        let member = if id == lead { "lead".to_owned() } else { id };
                        self.load_transcript(project, member);
                    }
                    // An open from a chat (F7, F8) has no panel: the thread view mounts over it.
                    None => {
                        if let Some(project) =
                            self.captain_context.as_ref().map(|c| c.project.clone())
                        {
                            self.open_provider_thread(view, project, id, title);
                        }
                    }
                }
            }
            OpenTarget::Factory => {
                view.overlay = None;
                self.open_factory_page(view);
            }
            OpenTarget::Draft(draft) => {
                // The draft goes above what the chat already holds.
                view.overlay = None;
                prepend_draft(view, &draft);
            }
        }
        self.dirty = true;
    }

    /// Open a session file in this same view: the draft of the chat being left is stashed
    /// for its return, and the opened chat's own stashed draft comes back.
    /// Open the chat a Captain action or a finished turn asked for, if any. The open runs from a
    /// background poll, so a failure surfaces as an error row and the current chat stays usable.
    pub(crate) async fn open_pending_captain_session(&mut self, view: &mut AgentView) {
        let Some(open) = self.pending_captain_open.take() else {
            return;
        };
        if let Err(error) = self.open_captain_session(view, open).await {
            self.error_row(&format!("Could not open that chat: {error:#}"), view);
        }
    }

    pub(crate) async fn open_captain_session(
        &mut self,
        view: &mut AgentView,
        open: CaptainOpen,
    ) -> anyhow::Result<()> {
        let CaptainOpen {
            path,
            draft,
            factory,
            retry,
        } = open;
        if self
            .session_file
            .as_deref()
            .is_some_and(|current| same_file(current, &path.to_string_lossy()))
        {
            if factory {
                self.open_factory_page(view);
            } else {
                if matches!(view.overlay, Some(Overlay::Factory(_))) {
                    self.factory_selected_run = None;
                    self.factory_view_session = None;
                }
                view.overlay = None;
            }
            if let Some(draft) = draft {
                prepend_draft(view, &draft);
            }
            self.dirty = true;
            return Ok(());
        }
        let selection = SessionSelection::Resume(path.clone());
        let id = match create_session(&self.client, &self.create_options(), Some(&selection)).await
        {
            Ok(id) => id,
            Err(error) => {
                match busy_open_note(&format!("{error:#}"), retry) {
                    BusyOpen::Wait(note) => {
                        // The draft and the editor stay as they are; the chat opens later.
                        self.captain_busy_open = Some(CaptainOpen {
                            path,
                            draft,
                            factory,
                            retry,
                        });
                        if let Some(note) = note {
                            self.note(note, view);
                        }
                    }
                    BusyOpen::Failed => {
                        self.error_row(&format!("Could not open that chat: {error:#}"), view)
                    }
                }
                return Ok(());
            }
        };
        self.stash_draft_for_switch(view);
        self.attach_session(&id, super::DockFold::FirstFrame)
            .await?;
        self.rebuild_view(view, &super::RebuildKind::Rebind);
        self.restore_prompt_stash_if_editor_empty(view, true);
        if let Some(draft) = draft {
            prepend_draft(view, &draft);
        }
        if factory {
            self.open_factory_page(view);
        } else {
            if matches!(view.overlay, Some(Overlay::Factory(_))) {
                self.factory_selected_run = None;
                self.factory_view_session = None;
            }
            view.close_overlay_if(|overlay| matches!(overlay, Overlay::Factory(_)));
        }
        // The breadcrumb and the Captain routing follow the opened chat now, before the
        // status call answers, so a message typed at once still reaches the Captain.
        self.captain_context = self
            .captain_snapshot
            .as_ref()
            .and_then(|snapshot| self.context_for(snapshot, &path.to_string_lossy()));
        self.sync_captain_active_project(view);
        self.sync_captain_header(view);
        self.poll_captain_status(view);
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
        // Inside a subagent no listed chat is the open one.
        let current = if context.subagent.is_some() {
            String::new()
        } else {
            context.chat.clone()
        };
        let mut picker = crate::captain_view::CrewPicker::new(
            context.project_name.clone(),
            current,
            context.chats,
        );
        if let Some(project) = self
            .captain_snapshot
            .as_ref()
            .and_then(|s| s.projects.iter().find(|p| p.id == context.project))
        {
            picker.set_project(project);
        }
        view.overlay = Some(Overlay::CrewPicker(picker));
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
                self.open_crew_chat(view, context.project, chat.clone());
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
        if context.chat == "captain" && context.subagent.is_none() {
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
                self.open_crew_chat(view, project, chat);
            }
            crate::captain_view::CrewPick::NewThread => {
                view.overlay = None;
                let Some(project) = self.captain_context.as_ref().map(|c| c.project.clone()) else {
                    return;
                };
                self.note("Opening a new crew chat…", view);
                self.run_captain_action(crate::captain_view::new_thread_action(project), view);
            }
            crate::captain_view::CrewPick::AddExisting => {
                let Some(project) = self.captain_context.as_ref().map(|c| c.project.clone()) else {
                    view.overlay = None;
                    return;
                };
                let mut panel = self.captain_parked.take().unwrap_or_else(|| {
                    CaptainView::new(picker_viewport_rows(view.terminal_rows()) + 8)
                });
                let action = panel.start_adopt(project);
                view.overlay = Some(Overlay::Captain(panel));
                self.refresh_captain();
                self.run_captain_action(action, view);
            }
        }
    }

    /// Open one chat of the switcher (F7, F8): a native chat in place, a provider CLI thread
    /// in the panel's thread view, or a factory run on the live factory page. The last two
    /// return to the chat they were opened over.
    fn open_crew_chat(&mut self, view: &mut AgentView, project: String, chat: CrewChat) {
        match crew_route(&chat) {
            CrewRoute::Run(member) => {
                self.open_captain_run(view, project, member, chat.run_id.clone());
            }
            CrewRoute::Thread(member) => {
                self.open_provider_thread(view, project, member, chat.label.clone());
            }
            CrewRoute::Chat(member) => {
                self.run_captain_action(CaptainAction::Open { project, member }, view);
            }
        }
    }

    /// Mount the panel's thread view on a provider CLI thread over the open chat; Esc closes
    /// back to it.
    fn open_provider_thread(
        &mut self,
        view: &mut AgentView,
        project: String,
        member: String,
        title: String,
    ) {
        let mut panel = CaptainView::new(picker_viewport_rows(view.terminal_rows()) + 8);
        panel.set_transient();
        panel.begin_thread(project.clone(), member.clone(), title);
        view.overlay = Some(Overlay::Captain(panel));
        Self::mark_opened_directly(view);
        self.load_transcript(project, member);
        self.refresh_captain();
        self.dirty = true;
    }

    /// Resolve a Captain-started factory run's host session (`sessionFile` -> live daemon
    /// `activeSessionId` in `self.roster`) so the Factory page inspects the run's actual
    /// host session instead of showing "No live factory runs." on the Captain's lead chat.
    pub(crate) fn resolve_captain_factory_run(
        &self,
        project: &str,
        member: Option<&str>,
    ) -> Option<ResolvedCaptainRun> {
        let proj = self
            .captain_snapshot
            .as_ref()?
            .projects
            .iter()
            .find(|p| p.id == project)?;
        let chosen = match member {
            Some(id) => proj.crew.iter().find(|m| m.id == id && m.is_run())?,
            None => proj
                .crew
                .iter()
                .filter(|m| m.is_run() && !m.archived)
                .find(|m| {
                    matches!(
                        m.run_state.as_deref(),
                        Some("running" | "paused" | "starting" | "queued")
                    ) || matches!(m.state.as_str(), "running" | "needs_you")
                })?,
        };
        let session_file = chosen
            .session
            .clone()
            .filter(|s| !s.trim().is_empty());
        let active_session_id = session_file.as_deref().and_then(|file| {
            self.roster
                .iter()
                .filter_map(|entry| entry.get("summary"))
                .find(|s| {
                    s.get("sessionFile")
                        .and_then(Value::as_str)
                        .is_some_and(|p| same_file(p, file))
                })
                .and_then(|s| s.get("activeSessionId").and_then(Value::as_str))
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        });
        let starting = matches!(chosen.run_state.as_deref(), Some("queued" | "starting"));
        Some(ResolvedCaptainRun {
            project: proj.id.clone(),
            member: chosen.id.clone(),
            label: chosen.label(),
            run_id: chosen.run_id.clone(),
            session_file,
            active_session_id,
            starting,
        })
    }

    /// The active Captain project's live or paused factory run, when viewing a Captain project.
    pub(crate) fn resolve_active_captain_factory_run(&self) -> Option<ResolvedCaptainRun> {
        let project = self
            .captain_context
            .as_ref()
            .map(|c| c.project.as_str())
            .or_else(|| {
                self.captain_dock
                    .as_ref()
                    .map(|d| d.project_id.as_str())
                    .filter(|s| !s.is_empty())
            })?;
        self.resolve_captain_factory_run(project, None)
    }

    /// Open the live factory page on a Captain-started run. From the panel, the page's
    /// close returns to it; from a chat or the dock, to where it was opened.
    pub(crate) fn open_captain_run(
        &mut self,
        view: &mut AgentView,
        project: String,
        member: String,
        run_id: Option<String>,
    ) {
        let from_panel =
            matches!(view.overlay, Some(Overlay::Captain(ref panel)) if !panel.is_transient());
        if !from_panel {
            // The page's close returns to a parked panel; with none it returns to its origin.
            self.captain_parked = None;
        }
        let resolved = self.resolve_captain_factory_run(&project, Some(&member));
        let run_id = run_id.or_else(|| resolved.as_ref().and_then(|r| r.run_id.clone()));
        if let Some(found) = &resolved {
            if let Some(active_id) = &found.active_session_id {
                self.factory_target_session_id = Some(active_id.clone());
            } else if !found.starting {
                if let Some(path) = &found.session_file {
                    self.open_captain_target(
                        OpenTarget::Session {
                            path: path.clone(),
                            factory: true,
                            draft: None,
                        },
                        view,
                    );
                    return;
                }
            }
        }
        if let Some(Overlay::Captain(panel)) = view.overlay.as_mut() {
            panel.park();
        }
        self.open_factory_page_from_captain(
            view,
            Some(crate::factory_view::CaptainRunOrigin {
                project,
                member: Some(member),
            }),
        );
        if let Some(Overlay::Factory(page)) = view.overlay.as_mut() {
            if let Some(found) = &resolved {
                if found.active_session_id.is_none() && found.starting {
                    page.set_error(Some(format!(
                        "Factory {} is starting — waiting for host kernel…",
                        found.label
                    )));
                }
            }
            if let Some(run_id) = run_id {
                page.select(crate::factory_view::FactoryRow {
                    run_id,
                    state_id: None,
                });
                self.sync_factory_selection(view);
            }
        }
        self.dirty = true;
    }

    /// `→` on a crew row with subagents: the scoped agents view on that crew chat's subtree.
    /// The panel parks, so F6 reopens on the same row.
    fn open_crew_subagents(&mut self, view: &mut AgentView, session: &str) {
        let summary = self
            .roster
            .iter()
            .filter_map(|entry| entry.get("summary"))
            .find(|summary| {
                summary
                    .get("sessionFile")
                    .and_then(Value::as_str)
                    .is_some_and(|path| same_file(path, session))
            })
            .cloned();
        let Some(summary) = summary else {
            if let Some(panel) = Self::panel_mut(view) {
                panel.set_notice("Its subagents are not in this daemon's roster yet.");
            }
            return;
        };
        if !self.return_to_agents_view {
            if let Some(panel) = Self::panel_mut(view) {
                panel.set_notice("The agents view needs a daemon-hosted session.");
            }
            return;
        }
        let text = |key: &str| {
            summary
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        if let Some(Overlay::Captain(mut panel)) = view.overlay.take() {
            panel.park();
            self.captain_parked = Some(panel);
        }
        self.scoped_agents_view = Some(crate::agents_view::AgentsViewScope {
            active_session_id: text("activeSessionId"),
            session_id: text("sessionId"),
            session_name: text("sessionName").or_else(|| text("name")),
        });
        self.open_agents_view = true;
        self.exit_requested = true;
        self.dirty = true;
    }
}

/// Where a switcher chat opens.
#[derive(Debug, PartialEq, Eq)]
enum CrewRoute {
    /// A Captain run: the live factory page.
    Run(String),
    /// A provider CLI thread: the panel's thread view over the chat.
    Thread(String),
    /// A native chat (the Captain when `None`): in place.
    Chat(Option<String>),
}

fn crew_route(chat: &CrewChat) -> CrewRoute {
    match chat.member.clone() {
        Some(member) if chat.kind == "factory" => CrewRoute::Run(member),
        Some(member) if chat.is_provider_thread() => CrewRoute::Thread(member),
        member => CrewRoute::Chat(member),
    }
}

/// The `edit` args for a Captain lead model picked in pa's shared picker. The args carry no
/// provider, so the backend keeps the Captain's current provider (Prime, Claude, Codex, or
/// Antigravity) and maps the `family/model` id onto it, or refuses a model it cannot run.
/// What a failed Captain chat open shows.
#[derive(Debug, PartialEq, Eq)]
enum BusyOpen {
    /// Another process holds the session for a turn: wait, with a note the first time.
    Wait(Option<&'static str>),
    Failed,
}

/// Put `draft` above what the editor already holds.
fn prepend_draft(view: &mut AgentView, draft: &str) {
    let current = view.editor.get_expanded_text();
    view.editor.set_text(&joined_draft(draft, &current));
}

fn joined_draft(draft: &str, current: &str) -> String {
    if current.trim().is_empty() {
        draft.to_owned()
    } else if draft.trim().is_empty() || current.starts_with(draft) {
        current.to_owned()
    } else {
        format!("{draft}\n\n{current}")
    }
}

/// A Captain status changed: a chat that waited for a turn is tried again, as a retry so its
/// notice is not shown twice. A chat already waiting to open goes first.
fn retry_busy_open(pending: &mut Option<CaptainOpen>, busy: &mut Option<CaptainOpen>) {
    if pending.is_none() {
        *pending = busy.take().map(|open| CaptainOpen {
            retry: true,
            ..open
        });
    }
}

/// The note for a failed open. A turn that holds the session's runtime lease (an older process
/// that runs it outside the daemon) is not an error: the chat opens when the turn ends.
fn busy_open_note(error: &str, retry: bool) -> BusyOpen {
    if !error.contains("runtime lease") {
        return BusyOpen::Failed;
    }
    BusyOpen::Wait((!retry).then_some("The Captain is working · opens when this turn ends"))
}

/// The `act lead` / `act steer` input for one Captain chat message.
fn captain_lead_args(lead: &CaptainLead) -> Value {
    let mut args = serde_json::json!({"project": lead.project, "text": lead.text});
    if lead.op == "steer" {
        args["request_id"] = Value::String(uuid::Uuid::new_v4().to_string());
    }
    if !lead.images.is_empty() {
        args["images"] = serde_json::json!(lead.images);
    }
    args
}

fn captain_lead_model_args(
    project: String,
    full_model: String,
    effort_edited: bool,
    thinking: String,
) -> Value {
    let mut args = serde_json::json!({
        "project": project,
        "field": "model",
        "value": full_model,
    });
    if effort_edited {
        args["thinking"] = Value::String(thinking);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_native_open_reply_opens_the_live_step_session() {
        let reply = json!({"ok": true, "kind": "native", "session": "/s/step.jsonl", "cwd": "/w"});
        assert_eq!(
            native_open_target(&reply),
            Some(OpenTarget::Session {
                path: "/s/step.jsonl".into(),
                factory: false,
                draft: None,
            })
        );
        assert_eq!(
            native_open_target(&json!({"kind": "thread", "session": "x"})),
            None
        );
        assert_eq!(
            native_open_target(&json!({"kind": "native", "session": " "})),
            None
        );
    }

    #[test]
    fn a_steer_names_its_delivery_and_an_old_backend_falls_back() {
        assert_eq!(
            captain_delivery_note("steer", &json!({"steered": true})),
            STEERED_NOTE
        );
        assert_eq!(
            captain_delivery_note("steer", &json!({"steered": false, "queued": true})),
            QUEUED_NOTE
        );
        assert_eq!(captain_delivery_note("lead", &json!({})), QUEUED_NOTE);
        assert!(steer_unsupported("Unknown captain action: steer"));
        assert!(steer_unsupported("argument op: invalid choice: 'steer'"));
        assert!(!steer_unsupported("The Captain is stopped"));
    }

    #[test]
    fn captain_lead_args_carry_images_and_a_steer_request_id() {
        let mut lead = CaptainLead {
            project: "p1".into(),
            text: "look".into(),
            done: String::new(),
            op: "lead",
            images: Vec::new(),
        };
        assert_eq!(
            captain_lead_args(&lead),
            json!({"project": "p1", "text": "look"})
        );
        lead.op = "steer";
        lead.images = vec!["/hub/captains/attachments/a.png".into()];
        let args = captain_lead_args(&lead);
        assert_eq!(args["images"], json!(["/hub/captains/attachments/a.png"]));
        assert!(args["request_id"].as_str().is_some_and(|id| !id.is_empty()));
    }

    #[test]
    fn captain_images_are_saved_as_hub_files() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let image = LoadedImage {
            data: base64::engine::general_purpose::STANDARD.encode(b"\x89PNG"),
            mime_type: "image/jpeg".into(),
        };
        let paths = save_captain_images(&dir.path().join("attachments"), &[&image]).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(
            Path::new(&paths[0])
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("jpg"))
        );
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"\x89PNG");
    }

    #[test]
    fn a_lease_conflict_waits_for_the_turn_without_the_daemon_text() {
        let error = "the daemon rejected the create request: This session is currently open in another Rust build of Prime Agent (active in pid 7420) — another daemon or window of this product holds the file's runtime lease.";
        assert_eq!(
            busy_open_note(error, false),
            BusyOpen::Wait(Some("The Captain is working · opens when this turn ends"))
        );
        // A retry that is still blocked waits quietly.
        assert_eq!(busy_open_note(error, true), BusyOpen::Wait(None));
        assert_eq!(busy_open_note("No such session", false), BusyOpen::Failed);
    }

    #[test]
    fn a_status_change_retries_a_waiting_open_once_quietly() {
        let open = CaptainOpen {
            path: "/s/lead.jsonl".into(),
            draft: Some("hi".into()),
            factory: false,
            retry: false,
        };
        let mut pending = None;
        let mut busy = Some(open.clone());
        retry_busy_open(&mut pending, &mut busy);
        assert_eq!(busy, None);
        assert_eq!(
            pending,
            Some(CaptainOpen {
                retry: true,
                ..open.clone()
            })
        );
        // A chat already waiting to open is not replaced.
        let mut busy = Some(CaptainOpen {
            path: "/s/c1.jsonl".into(),
            ..open
        });
        retry_busy_open(&mut pending, &mut busy);
        assert_eq!(pending.unwrap().path, PathBuf::from("/s/lead.jsonl"));
        assert!(busy.is_some());
    }

    #[test]
    fn a_draft_goes_above_the_editor_text_once() {
        assert_eq!(joined_draft("draft", ""), "draft");
        assert_eq!(joined_draft("draft", "typed"), "draft\n\ntyped");
        assert_eq!(joined_draft("draft", "draft\n\ntyped"), "draft\n\ntyped");
        assert_eq!(joined_draft("", "typed"), "typed");
    }

    #[test]
    fn a_lead_model_pick_keeps_the_captain_provider() {
        let args = captain_lead_model_args(
            "p1".into(),
            "anthropic/claude-opus-4-6".into(),
            false,
            "high".into(),
        );
        assert_eq!(
            args,
            json!({"project": "p1", "field": "model", "value": "anthropic/claude-opus-4-6"})
        );
        let args = captain_lead_model_args("p1".into(), "m".into(), true, "low".into());
        assert_eq!(args["thinking"], "low");
        assert!(args.get("provider").is_none());
    }

    fn snapshot() -> CaptainSnapshot {
        serde_json::from_value(json!({
            "current_root": "/p",
            "projects": [{
                "id": "p1", "name": "parser", "root": "/p", "lead": "l1",
                "lead_session": "/s/lead.jsonl", "lead_busy": false,
                "crew": [
                    {"id": "c1", "kind": "thread", "title": "parser-fix", "session": "/s/c1.jsonl", "busy": true},
                    {"id": "c2", "kind": "thread", "title": "claude review", "provider": "claude"},
                    {"id": "f1", "kind": "factory", "title": "check · x", "machine": "check",
                     "session": "/s/f.jsonl", "run_state": "paused", "state": "needs_you", "run_id": "r1"},
                    {"id": "f2", "kind": "factory", "title": "old · y", "run_state": "done", "state": "done"}
                ]
            }]
        }))
        .unwrap()
    }

    #[test]
    fn a_crew_chat_resolves_its_project_and_breadcrumb() {
        let context = captain_context(&snapshot(), "/s/c1.jsonl", &[]).unwrap();
        assert_eq!(context.project, "p1");
        assert_eq!(context.chat, "parser-fix");
        assert_eq!(context.subagent, None);
        assert_eq!(
            context
                .chats
                .iter()
                .map(|c| (c.label.as_str(), c.kind.as_str(), c.status.as_str()))
                .collect::<Vec<_>>(),
            [
                ("captain", "captain", "idle"),
                ("parser-fix", "chat", "working"),
                ("claude review", "claude", "idle"),
                ("check", "factory", "paused"),
            ],
            "provider threads and live runs are switchable"
        );
        assert!(context.chats[2].is_provider_thread());
        assert!(!context.chats[3].is_provider_thread());
        assert_eq!(
            context.chats[3].session, None,
            "a run opens its factory page"
        );
        assert_eq!(
            breadcrumb(&context, "F7", "Shift+F7", "F6"),
            "parser › parser-fix · F7 switch · Shift+F7 captain · F6 overview"
        );
        let lead = captain_context(&snapshot(), "/s/lead.jsonl", &[]).unwrap();
        assert_eq!(
            breadcrumb(&lead, "F7", "Shift+F7", "F6"),
            "parser › captain · F7 switch · F6 overview",
            "the Captain chat names no return-to-Captain key"
        );
        let mut busy_lead = lead.clone();
        busy_lead.lead_status = "working".into();
        assert_eq!(
            breadcrumb(&busy_lead, "F7", "Shift+F7", "F6"),
            "parser › captain · running · F7 switch · F6 overview"
        );
        busy_lead.lead_queued = vec!["first".into(), "second".into()];
        assert_eq!(
            breadcrumb(&busy_lead, "F7", "Shift+F7", "F6"),
            "parser › captain · running (2 queued) · F7 switch · F6 overview"
        );
        busy_lead.lead_status = "needs_you".into();
        busy_lead.lead_queued.clear();
        assert_eq!(
            breadcrumb(&busy_lead, "F7", "Shift+F7", "F6"),
            "parser › captain · needs you · F7 switch · F6 overview"
        );
        assert!(captain_context(&snapshot(), "/s/other.jsonl", &[]).is_none());
    }

    #[test]
    fn the_switcher_routes_runs_provider_threads_and_chats() {
        let context = captain_context(&snapshot(), "/s/lead.jsonl", &[]).unwrap();
        let routes: Vec<CrewRoute> = context.chats.iter().map(crew_route).collect();
        assert_eq!(
            routes,
            [
                CrewRoute::Chat(None),
                CrewRoute::Chat(Some("c1".into())),
                CrewRoute::Thread("c2".into()),
                CrewRoute::Run("f1".into()),
            ]
        );
        assert_eq!(context.chats[3].run_id.as_deref(), Some("r1"));
    }

    #[test]
    fn a_subagent_of_a_crew_chat_keeps_its_captain_context() {
        let roster = vec![
            json!({"summary": {"sessionFile": "/s/sub2.jsonl", "sessionName": "grammar-check",
                "parentSessionPath": "/s/sub1.jsonl"}}),
            json!({"summary": {"sessionFile": "/s/sub1.jsonl", "sessionName": "lexer",
                "parentSessionPath": "/s/c1.jsonl"}}),
            json!({"summary": {"sessionFile": "/s/c1.jsonl"}}),
        ];
        let ancestors = session_ancestors(&roster, "/s/sub2.jsonl");
        assert_eq!(
            ancestors,
            [
                ("/s/sub1.jsonl".to_owned(), "grammar-check".to_owned()),
                ("/s/c1.jsonl".to_owned(), "lexer".to_owned()),
            ]
        );
        let context = captain_context(&snapshot(), "/s/sub2.jsonl", &ancestors).unwrap();
        assert_eq!(context.project, "p1");
        assert_eq!(context.chat, "parser-fix");
        assert_eq!(context.subagent.as_deref(), Some("grammar-check"));
        assert_eq!(
            breadcrumb(&context, "F7", "Shift+F7", "F6"),
            "parser › parser-fix › grammar-check · F7 switch · Shift+F7 captain · F6 overview"
        );
        // A subagent of the Captain chat still offers the way back to the Captain.
        let roster = vec![json!({"summary": {"sessionFile": "/s/x.jsonl",
            "parentSessionPath": "/s/lead.jsonl"}})];
        let ancestors = session_ancestors(&roster, "/s/x.jsonl");
        let context = captain_context(&snapshot(), "/s/x.jsonl", &ancestors).unwrap();
        assert_eq!(context.chat, "captain");
        assert!(breadcrumb(&context, "F7", "Shift+F7", "F6").contains("Shift+F7 captain"));
    }
}
