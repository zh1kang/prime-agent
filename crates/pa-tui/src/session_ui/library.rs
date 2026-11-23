//! The machine library page's session surface: the page open (the
//! activity dock's machine-library group's destination), the view's key
//! loop, and the daemon `factory_activity` lane the page's fetches ride
//! (the `library` action: the machine list on open, one machine's graph
//! payload on the drill-in). With no kernel running, both fetches fall
//! back to `pa thread fsm-edit` on disk: the catalog list, and a
//! throwaway draft's compiled graph and per-state files.

use std::time::Duration;

use anyhow::Result;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;

use pa_types::daemon::DaemonCommand;

use super::AgentView;
use crate::view::Overlay;

use crate::factory_view::{
    parse_library_machines, LibraryView, LibraryViewAction, MALFORMED_LIBRARY_REPLY_ERROR,
};
use crate::session_ui::{picker_viewport_rows, UI_REQUEST_TIMEOUT_MS};

/// A background list or graph reply, fenced by its originating session and request epoch.
pub(crate) enum LibraryUpdate {
    Machines {
        session: String,
        epoch: u64,
        data: Value,
    },
    Error {
        session: String,
        epoch: u64,
        message: String,
    },
    Graph {
        /// Durable session identity; a reattach may replace its wire id.
        session: String,
        epoch: u64,
        name: String,
        result: std::result::Result<Value, String>,
    },
}

impl super::SessionUi {
    /// Open the machine library page: the activity dock's
    /// machine-library group's destination (the factory page's sibling —
    /// the same lane, the same opt-in gate). The keypress never waits on
    /// the daemon (the factory page's pattern): the view mounts at once
    /// and the list fetch lands through the fold; a daemon without the
    /// lane reports exactly that instead of mounting nothing silently.
    pub(crate) fn open_library_page(&mut self, view: &mut AgentView) {
        if !self.factory_activity_supported() {
            self.note(
                "The machine library page needs a daemon that advertises the factory lane",
                view,
            );
            return;
        }
        // The mounted view belongs to this session: the epoch guard drops
        // a late reply from a previous session's open.
        self.library_list_epoch = self.library_list_epoch.wrapping_add(1);
        self.library_detail_epoch = self.library_detail_epoch.wrapping_add(1);
        self.library_view_session = Some(self.session_id.clone());
        self.factory_selected_run = None;
        self.factory_view_session = None;
        self.panel_returns_to_agents_view = false;
        view.overlay = Some(Overlay::Library(LibraryView::loading(
            picker_viewport_rows(view.terminal_rows()),
        )));
        self.dirty = true;
        self.spawn_library_list_refresh();
    }

    /// One key press while the view is open: the view resolves the key;
    /// the drill-in rides the daemon lane (bounded, with the outcome on
    /// the view's error row), and Esc closes.
    pub(crate) fn handle_library_view_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = super::key_event_to_id(&key) else {
            return Ok(());
        };
        let selected_before = match view.overlay.as_ref() {
            Some(Overlay::Library(library_view)) => library_view
                .selected_machine()
                .map(|machine| machine.name.clone()),
            _ => None,
        };
        let action = match view.overlay.as_mut() {
            Some(Overlay::Library(library_view)) => {
                Some(library_view.handle_key(&id, view.editor.keybindings()))
            }
            _ => None,
        };
        let selected_after = match view.overlay.as_ref() {
            Some(Overlay::Library(library_view)) => library_view
                .selected_machine()
                .map(|machine| machine.name.as_str()),
            _ => None,
        };
        // Navigation mutates selection and scroll even without a daemon action.
        self.dirty = true;
        let selection_changed = selected_before.as_deref() != selected_after;
        if selection_changed {
            if let Some(Overlay::Library(library_view)) = view.overlay.as_mut() {
                library_view.set_error(None);
            }
        }
        if selection_changed
            || matches!(
                action,
                Some(
                    LibraryViewAction::Close
                        | LibraryViewAction::Back
                        | LibraryViewAction::DrillIn { .. }
                )
            )
        {
            self.library_detail_epoch = self.library_detail_epoch.wrapping_add(1);
        }
        match action {
            Some(LibraryViewAction::None) | None => {}
            // Backing out to the list fences the drill-in's pending reply;
            // the epoch bump above is what drops it.
            Some(LibraryViewAction::Back) => {}
            Some(LibraryViewAction::Close) => {
                if matches!(view.overlay, Some(Overlay::Library(_))) {
                    self.close_overlay_to_origin(view);
                }
                self.library_view_session = None;
            }
            Some(LibraryViewAction::DrillIn { name }) => {
                self.spawn_library_graph_fetch(name);
            }
            Some(LibraryViewAction::Edit { name }) => {
                view.close_overlay_if(|overlay| matches!(overlay, Overlay::Library(_)));
                self.library_view_session = None;
                self.open_workspace(crate::workspace_view::WorkspaceView::machines(&name), view);
            }
            Some(LibraryViewAction::Run { .. }) => {
                view.close_overlay_if(|overlay| matches!(overlay, Overlay::Library(_)));
                self.library_view_session = None;
                self.open_factory_launch(view);
            }
        }
        Ok(())
    }

    /// What a background `pa thread` call needs from this session.
    fn pa_thread(&self) -> PaThread {
        PaThread {
            cwd: self.cwd.clone(),
            session_file: self.session_file.clone(),
            socket: self.daemon_socket.clone(),
        }
    }

    /// Fetch a graph off the input path. Selection changes and page mounts fence its reply.
    pub(crate) fn spawn_library_graph_fetch(&self, name: String) {
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        let session = self.session_id.clone();
        let epoch = self.library_detail_epoch;
        let tx = self.library_updates.clone();
        let pa = self.pa_thread();
        tokio::spawn(async move {
            let fetched = client.request_ok(DaemonCommand::FactoryActivity {
                id: None,
                active_session_id,
                action: "library".to_string(),
                run_id: None,
                spec_id: Some(name.clone()),
                timeout_ms: None,
                rest: Map::default(),
            });
            let result =
                match tokio::time::timeout(Duration::from_millis(UI_REQUEST_TIMEOUT_MS), fetched)
                    .await
                {
                    Ok(Err(error)) if crate::daemon_client::is_kernel_not_running(&error) => {
                        pa.machine_graph(&name).await
                    }
                    Ok(reply) => reply.map_err(|error| format!("{error:#}")),
                    Err(_) => {
                        Err("timed out waiting for the Prime Agent daemon response".to_string())
                    }
                };
            let _ = tx.send(LibraryUpdate::Graph {
                session,
                epoch,
                name,
                result,
            });
        });
    }

    /// Fire the open path's list fetch (the heartbeat refresh's shape,
    /// one-shot: the library changes only through `prime-agent factory
    /// import | export`, so an open refetches rather than a standing
    /// poll). The reply lands through the run loop's channel; a stale
    /// epoch (a later open already fired) drops at fold time.
    pub(crate) fn spawn_library_list_refresh(&mut self) {
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        let epoch = self.library_list_epoch;
        let tx = self.library_updates.clone();
        let pa = self.pa_thread();
        tokio::spawn(async move {
            let fetched = client.request_ok(DaemonCommand::FactoryActivity {
                id: None,
                active_session_id: active_session_id.clone(),
                action: "library".to_string(),
                run_id: None,
                spec_id: None,
                timeout_ms: None,
                rest: Map::default(),
            });
            let fetched =
                tokio::time::timeout(Duration::from_millis(UI_REQUEST_TIMEOUT_MS), fetched).await;
            let update = match fetched {
                Ok(Ok(data)) => LibraryUpdate::Machines {
                    session: active_session_id,
                    epoch,
                    data,
                },
                Ok(Err(error)) if crate::daemon_client::is_kernel_not_running(&error) => {
                    match pa.run(&["fsm-edit", "list"], &serde_json::json!({})).await {
                        Ok(data) => LibraryUpdate::Machines {
                            session: active_session_id,
                            epoch,
                            data,
                        },
                        Err(message) => LibraryUpdate::Error {
                            session: active_session_id,
                            epoch,
                            message,
                        },
                    }
                }
                Ok(Err(error)) => LibraryUpdate::Error {
                    session: active_session_id,
                    epoch,
                    message: format!("{error:#}"),
                },
                Err(_) => LibraryUpdate::Error {
                    session: active_session_id,
                    epoch,
                    message: "timed out waiting for the Prime Agent daemon response".to_string(),
                },
            };
            let _ = tx.send(update);
        });
    }

    /// Fold one library fetch delivery into the session: a foreign session
    /// or a stale epoch (a later open already fired) drops; the open view
    /// absorbs the machines (a malformed reply keeps the rows and
    /// reports the lane instead of painting a fake empty state), and an
    /// error lands on the view's error row.
    pub(crate) fn apply_library_update(&mut self, update: LibraryUpdate, view: &mut AgentView) {
        let (session, epoch, payload) = match update {
            LibraryUpdate::Graph {
                session,
                epoch,
                name,
                result,
            } => {
                // Reattachment changes the wire id, not the mounted machine's owner.
                if session != self.session_id || epoch != self.library_detail_epoch {
                    return;
                }
                if let Some(Overlay::Library(library_view)) = view.overlay.as_mut() {
                    match result {
                        Ok(data) => library_view.apply_detail(&name, &data),
                        Err(message) => library_view.set_error(Some(message)),
                    }
                    self.dirty = true;
                }
                return;
            }
            LibraryUpdate::Machines {
                session,
                epoch,
                data,
            } => (session, epoch, Ok(data)),
            LibraryUpdate::Error {
                session,
                epoch,
                message,
            } => (session, epoch, Err(message)),
        };
        if session != self.active_session_id || epoch != self.library_list_epoch {
            return;
        }
        if let Some(Overlay::Library(library_view)) = view.overlay.as_mut() {
            match payload {
                Ok(data) => {
                    if crate::factory_view::library_reply_lists_machines(&data) {
                        let selected_before = library_view
                            .selected_machine()
                            .map(|machine| machine.name.clone());
                        library_view.set_list_error(None);
                        library_view.apply_machines(parse_library_machines(&data));
                        let selected_after = library_view
                            .selected_machine()
                            .map(|machine| machine.name.as_str());
                        if selected_before.as_deref() != selected_after {
                            library_view.set_error(None);
                            self.library_detail_epoch = self.library_detail_epoch.wrapping_add(1);
                        }
                    } else {
                        library_view
                            .set_list_error(Some(MALFORMED_LIBRARY_REPLY_ERROR.to_string()));
                    }
                }
                Err(message) => {
                    library_view.set_list_error(Some(message));
                }
            }
        }
        self.dirty = true;
    }
}

/// A background `pa thread` call's context: the chat's directory, session
/// file, and daemon socket.
struct PaThread {
    cwd: std::path::PathBuf,
    session_file: Option<String>,
    socket: std::path::PathBuf,
}

impl PaThread {
    /// Run `pa thread ARGS` with `input` as JSON on stdin and parse its JSON answer.
    async fn run(&self, args: &[&str], input: &Value) -> std::result::Result<Value, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut command = tokio::process::Command::new(exe);
        command
            .arg("thread")
            .args(args)
            .current_dir(&self.cwd)
            .env_remove("PA_CAPTAIN_ACTOR")
            .env("PA_CAPTAIN_NATIVE_SOCKET", &self.socket)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(session) = &self.session_file {
            command.env("PA_CAPTAIN_SESSION", session);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("Could not start pa: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(input.to_string().as_bytes())
                .await
                .map_err(|e| e.to_string())?;
        }
        let output = tokio::time::timeout(
            Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| "timed out reading the machine library from disk".to_string())?
        .map_err(|e| e.to_string())?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(if message.is_empty() {
                format!("pa thread {} exited with {}", args.join(" "), output.status)
            } else {
                message.lines().last().unwrap_or(message).to_owned()
            });
        }
        serde_json::from_slice::<Value>(&output.stdout)
            .map_err(|e| format!("pa thread returned invalid JSON: {e}"))
    }

    /// One machine's compiled graph and per-state files from disk: a
    /// throwaway `fsm-edit` draft, discarded as soon as it is read.
    async fn machine_graph(&self, name: &str) -> std::result::Result<Value, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let draft = self
            .run(
                &["fsm-edit", "draft"],
                &serde_json::json!({"name": name, "target": format!("pa-view-{nanos}")}),
            )
            .await?;
        if let Some(dir) = draft.get("draft").and_then(Value::as_str) {
            let _ = self
                .run(&["fsm-edit", "discard"], &serde_json::json!({"draft": dir}))
                .await;
        }
        Ok(draft)
    }
}
