//! The open/attach/reattach pipeline (the session selection's open, the
//! §10 reattach, the attach fold, and the transcript rebuild it feeds),
//! the stats refresh, and the detach/exit request helpers.
use std::path::PathBuf;

use super::{
    attach_capabilities, attach_data_from_response, create_session, last_assistant_text_of, mpsc,
    reconstruct, resume_hint_from_stats, ActivityUpdates, AgentView, BTreeMap, ChatEntry,
    CompactionAbortNote, CompactionReason, CompactionState, Context, DaemonClient, DaemonCommand,
    DockFold, Duration, GoalView, HashSet, InteractiveOptions, LoaderTokenTracker, Map,
    ModelCatalogUpdate, Overlay, PromptOrder, PromptSubmitNote, ReattachOutcome, RebuildKind,
    RecoveryKind, ReloadNote, Result, ResyncBash, SessionSelection, SessionUi, ShareNote,
    TranscriptPageNote, UpdateNote, Value, EXIT_DETACH_TIMEOUT_MS, EXIT_STATS_TIMEOUT_MS,
    UI_REQUEST_TIMEOUT_MS,
};

impl SessionUi {
    /// Create/attach per the session selection and return the live state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open(
        client: DaemonClient,
        options: &InteractiveOptions,
        notes: mpsc::UnboundedSender<String>,
        compaction_abort_notes: mpsc::UnboundedSender<CompactionAbortNote>,
        prompt_notes: mpsc::UnboundedSender<PromptSubmitNote>,
        share_notes: mpsc::UnboundedSender<ShareNote>,
        reload_notes: mpsc::UnboundedSender<ReloadNote>,
        update_notes: mpsc::UnboundedSender<UpdateNote>,
        traces_upload_notes: mpsc::UnboundedSender<crate::traces::TraceUploadAllNote>,
        catalog_updates: mpsc::UnboundedSender<ModelCatalogUpdate>,
        auth_panel_notes: mpsc::UnboundedSender<crate::auth_panel::AuthPanelRequest>,
        activity_updates: ActivityUpdates,
        page_notes: mpsc::UnboundedSender<TranscriptPageNote>,
    ) -> Result<SessionUi> {
        let active_session_id = match &options.session {
            SessionSelection::New => create_session(&client, options, None).await?,
            SessionSelection::NewChild { rlm_depth, .. } => {
                let id = create_session(&client, options, None).await?;
                // `tui agents new scoped`, fire-and-forget (the
                // `subagents_view_opened` pattern): the open never waits on
                // the telemetry flush.
                if let Some(telemetry) = options.telemetry.clone() {
                    let depth = *rlm_depth;
                    tokio::spawn(async move {
                        telemetry.scoped_agent_created(depth).await;
                    });
                }
                id
            }
            SessionSelection::Attach(id) => id.clone(),
            SessionSelection::Resume(_) | SessionSelection::ResumeWith { .. } => {
                create_session(&client, options, Some(&options.session)).await?
            }
        };
        // The single prompt-submit worker (see [`PromptOrder`]) lives with the orders channel: the
        // session drops its sender, so the task never outlives the run.
        let (orders_tx, orders_rx) = mpsc::unbounded_channel::<PromptOrder>();
        tokio::spawn(Self::prompt_submit_worker(orders_rx, prompt_notes.clone()));
        let mut session = SessionUi {
            client,
            active_session_id: String::new(),
            session_id: String::new(),
            attach_event_generation: String::new(),
            attach_event_sequence: 0,
            last_event_sequence: 0,
            attach_cursor_present: false,
            session_name: None,
            cwd: options.cwd.clone(),
            session_dir: options.session_dir.clone(),
            script_path: options.script_path.clone(),
            model_selection: options.model_selection.clone(),
            models: options.models.clone(),
            model_catalog: options.model_catalog.clone(),
            model_configured_providers: options.model_configured_providers.clone(),
            model_recent_models: options.model_recent_models.clone(),
            default_thinking_level: options.default_thinking_level.clone(),
            models_fetched_at: None,
            catalog_updates,
            heartbeat_updates: activity_updates.heartbeats,
            telemetry_disabled: options.telemetry_disabled,
            code_block_indent: options.code_block_indent.clone(),
            tree_filter_mode: crate::tree_list::filter_mode_from_str(&options.tree_filter_mode),
            branch_summary_skip_prompt: options.branch_summary_skip_prompt,
            last_status_index: None,
            show_images: options.show_images,
            fullscreen_mouse: options.fullscreen_mouse,
            service_tier: None,
            pending_model_fallback_message: None,
            speed_display_enabled: false,
            speed_stats: None,
            client_settings: options.client_settings.clone(),
            anthropic_subscription_warning_shown: false,
            anthropic_warning_mark_pending: std::sync::Arc::default(),
            active_side_question_id: None,
            side_question_counter: 0,
            share: None,
            reload: None,
            reload_notes,
            share_notes,
            trace_upload: None,
            traces_upload_notes,
            pending_traces_login: None,
            traces_login_run: None,
            traces_login_gen: 0,
            pasted_images: BTreeMap::default(),
            next_image_marker_id: 1,
            pending_snapshot: None,
            pending_streaming_message: None,
            captain_lead_sent_at: None,
            captain_queue_mirrored: false,
            trim_after_frame: false,
            pending_model: None,
            pending_model_provider: None,
            pending_thinking_suffix: None,
            pending_queue: None,
            queue_selection: crate::queued::QueueSelection::default(),
            context: None,
            list_rows: Vec::new(),
            turn_active: false,
            run_open: false,
            run_outcome: crate::program_status::Status::Done,
            outcome_after_compaction: None,
            settled: None,
            turn_ends_seen: 0,
            last_prompt_turn_end: 0,
            steering_mode: "all".to_string(),
            streaming_index: None,
            optimistic_user_echo: None,
            working_tokens: LoaderTokenTracker::default(),
            turn_error_shown: false,
            pending_tools: HashSet::default(),
            aborted_tools: HashSet::default(),
            last_assistant_text: None,
            osc_sink: crate::clipboard::OscSink::Stdout,
            traces: options.traces.clone(),
            provider_auth: options.provider_auth.clone(),
            pending_model_sign_in: None,
            auth_panel_notes,
            auth_panel_cancel: None,
            update_commands: options.update_commands.clone(),
            update_notes,
            update_in_flight: false,
            exit_requested: false,
            open_agents_view: false,
            scoped_agents_view: None,
            roster: Vec::new(),
            heartbeat_catalog: Vec::new(),
            heartbeat_refresh_in_flight: false,
            heartbeat_refresh_queued: false,
            heartbeat_refresh_epoch: 0,
            command_updates: activity_updates.commands,
            command_refresh_epoch: 0,
            skill_commands_cache: Vec::new(),
            bash_activities: serde_json::json!({"activities": []}),
            bash_list_epoch: 0,
            bash_updates: activity_updates.bash,
            factory_graph: serde_json::json!({"runs": []}),
            factory_view_session: None,
            factory_refresh_in_flight: false,
            factory_refresh_queued: false,
            factory_list_epoch: 0,
            factory_node_epoch: 0,
            factory_updates: activity_updates.factory,
            library_updates: activity_updates.library,
            library_list_epoch: 0,
            library_view_session: None,
            library_detail_epoch: 0,
            factory_selected_run: None,
            subagents_focused: false,
            activity_group: crate::chrome::ActivityGroup::Subagents,
            subagent_counts: crate::subagents::SubagentCounts::default(),
            session_file: None,
            pending_selection: None,
            pending_selection_rlm_depth: None,
            pending_selection_origin: None,
            return_to_agents_view: !options.no_session,
            panel_returns_to_agents_view: false,
            client_auth: options.client_auth.clone(),
            keybindings: options.keybindings.clone(),
            prompt_stash: options.prompt_stash.clone(),
            stash_session_id: String::new(),
            dirty: true,
            ctrl_c_hint_until: None,
            goal_view: GoalView::new(),
            notes,
            compaction_abort_notes,
            prompt_orders: orders_tx,
            input_submission_generation: 0,
            prompt_in_flight: 0,
            compaction_settled: false,
            window: super::window::TranscriptWindow::default(),
            page_notes,
            telemetry: options.telemetry.clone(),
            scroll_adoption_emitted: false,
            exit_reason: "daemon_closed",
            connection: crate::interactive::reconnect::Connection::Connected,
            pending_rebind: None,
            exit_guard: crate::exit_guard::ExitGuard::new(),
            escape_repeat_action: None,
            escape_repeat_until: None,
            escape_tree_shortcut_spent: false,
            user_bash_running: false,
            user_bash_card: None,
            user_bash_started_at: None,
            user_bash_counter: 0,
            resync_bash: None,
            attach_compacting: None,
            loader_anchor_ms: None,
            side_bash: None,
            side_bash_discarded: None,
            side_bash_counter: 0,
            suspend_requested: false,
            workspace_command: None,
            captain_updates: activity_updates.captain,
            usage_updates: activity_updates.usage,
            usage_cache: None,
            usage_fetch: None,
            usage_generation: 0,
            captain_compose: None,
            captain_context: None,
            captain_parked: None,
            workspace_parked: None,
            model_pick_target: super::model_picker::ModelPickTarget::Chat,
            captain_states: None,
            captain_status_watcher: crate::interactive::captain_hint::CaptainStatusWatcher::for_cwd(
                &options.cwd,
            ),
            captain_dock: None,
            captain_db_stamp: None,
            captain_refresh: super::captain::CaptainRefresh::Idle,
            captain_focus_query: None,
            captain_pending_key: None,
            pending_captain_open: None,
            captain_busy_open: None,
            daemon_socket: options.socket_path.clone(),
            captain_snapshot: None,
            captain_lead_inflight: None,
            captain_lead_queue: std::collections::VecDeque::new(),
            external_editor_request: None,
            pending_mcp_auth: None,
            picker_restored_draft: false,
            suspend_adoption_emitted: false,
            selection_auto_scroll: None,
            selection_adoption_emitted: false,
            copies: Vec::new(),
            pressed_hyperlink: None,
            left_mouse_dragged: false,
            opened_urls: Vec::new(),
            pressed_click: None,
            click_adoption_emitted: false,
        };
        session
            .attach_session(&active_session_id, DockFold::FirstFrame)
            .await
            .with_context(|| format!("attaching session {active_session_id}"))?;
        // The scope-back reopen's restore lands AFTER the attach: the attach's rebind reset clears
        // the focus, so the reopen's own restore must survive it.
        session.subagents_focused = options.restore_dock_focus;
        Ok(session)
    }

    /// Spec §10.2-§10.5: reattach after a restart — the fresh client replaces the dead one, the
    /// attach goes by DURABLE session id. `kind` names the driver: the update restart paints its
    /// §10.5 banner, a lost or announced-shutdown window reports the restart version-honestly.
    pub(crate) async fn reattach_after_recovery(
        &mut self,
        client: DaemonClient,
        view: &mut AgentView,
        kind: RecoveryKind,
    ) -> Result<ReattachOutcome> {
        // One reattach attempt's budget (§10.4: expiry is a RETRY outcome, never fatal). The bound
        // lives INSIDE this function — a caller-side timeout would cancel this future mid-attach
        // and skip the failure-path `close()`, leaking the half-installed client's connection.
        const REATTACH_BUDGET: Duration = Duration::from_secs(30);
        let hello_resume = client
            .hello()
            .get("updateResume")
            .cloned()
            .unwrap_or(Value::Null);
        let complete = hello_resume.get("complete").and_then(Value::as_bool);
        let update_id = hello_resume
            .get("updateId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.client = client;
        let durable = self.session_id.clone();
        if durable.is_empty() {
            // A failed reattach must not leave the half-installed client running:
            // close it; the reconnect driver installs a fresh one on its next attempt.
            self.client.hard_close();
            anyhow::bail!("the session's durable id is unknown; cannot reattach");
        }
        let anchor = self.reader_anchor(view);
        let attach = tokio::time::timeout(
            REATTACH_BUDGET,
            self.attach_session(&durable, DockFold::Held),
        )
        .await;
        match attach {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                // Same disposal: the reconnect driver installs a fresh one next attempt.
                self.client.hard_close();
                let what = match kind {
                    RecoveryKind::Update => "after the update",
                    RecoveryKind::Lost | RecoveryKind::Shutdown => "after the restart",
                };
                return Err(error.context(format!("reattaching session {durable} {what}")));
            }
            Err(_) => {
                // A wedged attach outlived the budget (§10.4): the expiry is a RETRY
                // outcome — the driver's next attempt owns the recovery.
                self.client.hard_close();
                return Ok(ReattachOutcome::AttachBudgetExceeded);
            }
        }
        // Flush the attach snapshot BEFORE the banner lands: the rebuild replaces the transcript
        // from the snapshot, so the banner must come after it to survive the rebuild.
        self.resync_view(anchor, view).await;
        match kind {
            RecoveryKind::Update => match complete {
                Some(false) => view.push_entry(crate::chat::ChatEntry::Status {
                    text: "Reconnected — the daemon is finishing its restore; queued work resumes when the session comes up.".to_string(),
                    kind: crate::chat::StatusKind::Info,
                }),
                _ => view.push_entry(crate::chat::ChatEntry::Status {
                    text: format!(
                        "Reconnected to radiata (update {update_id}) — your session and queued work resumed."
                    ),
                    kind: crate::chat::StatusKind::Info,
                }),
            },
            RecoveryKind::Lost | RecoveryKind::Shutdown => {
                // TS `formatDaemonReconnectBanner`: the recovered window reports the restart
                // version-honestly.
                let daemon_version = self
                    .client
                    .hello()
                    .get("appVersion")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let (text, status_kind) = crate::daemon_reconnect::reconnect_banner(
                    daemon_version.as_deref(),
                    env!("CARGO_PKG_VERSION"),
                );
                view.push_entry(crate::chat::ChatEntry::Status {
                    text,
                    kind: status_kind,
                });
            }
        }
        self.dirty = true;
        Ok(ReattachOutcome::Attached)
    }

    /// Detach the current session and attach `id`, rebuilding the transcript from the slim attach
    /// snapshot; a direct worker link serves the attach when the supervisor issues a ticket (a
    /// failed direct attach retries once over the supervisor). The NEW attach lands before the old
    /// id detaches: a failed re-attach must not strand the pane locally bound but server-detached.
    pub(crate) async fn attach_session(
        &mut self,
        active_session_id: &str,
        dock_fold: DockFold,
    ) -> Result<()> {
        let previous = self.active_session_id.clone();
        // A chat the user opened replaces a Captain chat still waiting for its turn to end.
        if previous != active_session_id {
            self.captain_busy_open = None;
        }
        // A direct link is bound to one session: drop it when switching.
        if self
            .client
            .direct_session_id()
            .is_some_and(|direct| direct != active_session_id)
        {
            self.client.drop_direct();
        }
        // The primary interactive connection sends its Herdr pane identity
        // with attach so an env-less session (e.g. cron-created) can adopt
        // it (adopt-if-absent, never rebind — the daemon owns that rule);
        // a client outside a Herdr pane sends nothing (the wire keeps its
        // tip shape).
        let client_env = {
            let env =
                radiata_types::daemon::herdr_env::collect_client_env(|key| std::env::var(key).ok());
            (!env.is_empty()).then_some(env)
        };
        let attach_command = |session_id: &str| DaemonCommand::Attach {
            id: None,
            active_session_id: session_id.to_string(),
            client_id: None,
            // `elide_snapshot_images`: the transcript arrives without the base64 image payloads, so
            // an image-heavy session's attach stops transferring megabytes.
            capabilities: Some(attach_capabilities()),
            resume_cursor: None,
            telemetry_disabled: self.telemetry_disabled.filter(|disabled| *disabled),
            recovery_config: None,
            env: client_env.clone(),
            launch_env: None,
            rest: Map::default(),
        };
        let direct_attached = self
            .client
            .upgrade_direct(active_session_id)
            .await
            .unwrap_or(false);
        let attached = match self
            .client
            .request_ok(attach_command(active_session_id))
            .await
        {
            Ok(data) => data,
            Err(error) => {
                if !direct_attached {
                    return Err(error);
                }
                self.client.drop_direct();
                self.client
                    .request_ok(attach_command(active_session_id))
                    .await?
            }
        };
        let data = attached;
        let attach = attach_data_from_response(data)?;
        // The bash slot follows the attached session's live state: the captured
        // state drives the next rebuild's resync edge.
        let state = attach.snapshot.get("state");
        let resync_bash = ResyncBash {
            was_running: self.user_bash_running,
            snap_running: state
                .and_then(|state| state.get("isBashRunning"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            snap_streaming: state
                .and_then(|state| state.get("isStreaming"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        self.user_bash_running = resync_bash.snap_running;
        self.resync_bash = Some(resync_bash);
        self.attach_compacting = Some(
            state
                .and_then(|state| state.get("isCompacting"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        let reconstructed = reconstruct(&attach);
        let mounted_session_changes = previous != attach.active_session_id;
        self.active_session_id = attach.active_session_id;
        // Retire the superseded id's subscription now, addressed by the captured previous id (the
        // detach must target the OLD address, not the newly adopted one).
        if !previous.is_empty() && previous != self.active_session_id {
            let _ = self
                .bounded_request(
                    Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                    DaemonCommand::Detach {
                        id: None,
                        active_session_id: Some(previous),
                        rest: Map::default(),
                    },
                )
                .await;
        }
        self.session_id = reconstructed.session_id;
        // `/review`, `@` completion, and every cwd-relative action follow the attached session's
        // own directory (a crew thread's worktree), never the launch checkout.
        if let Some(cwd) = attached_session_cwd(&attach.snapshot) {
            self.cwd = cwd;
        }
        self.attach_event_generation
            .clone_from(&reconstructed.event_generation);
        self.attach_event_sequence = reconstructed.last_event_sequence;
        // The live tracker starts at the attach's value; the cursor-presence gate
        // keeps a cursor-less attach from stashing a handoff.
        self.last_event_sequence = reconstructed.last_event_sequence;
        self.attach_cursor_present = reconstructed.cursor_present;
        self.connection
            .on(crate::interactive::reconnect::ConnectionEvent::Attached);
        self.session_name.clone_from(&reconstructed.session_name);
        self.service_tier.clone_from(&reconstructed.service_tier);
        self.pending_model_fallback_message = reconstructed.model_fallback_message;
        self.session_file = attach
            .snapshot
            .get("state")
            .and_then(|state| state.get("sessionFile"))
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
            .map(str::to_string);
        // The subagent summary follows the fresh session's family; the focus
        // returns to the editor.
        self.roster.clear();
        self.subagents_focused = false;
        self.subscribe_roster().await;
        // The dock's heartbeat rows follow `dock_fold`: a first-content-frame attach folds the
        // fresh fetch BEFORE the attach returns (the operator's 2026-09-26 zero-shift ruling).
        match dock_fold {
            DockFold::FirstFrame | DockFold::Fresh => self.heartbeat_catalog.clear(),
            // The held dock keeps its data: an already-up surface's dock
            // must not flicker away while the background refresh runs.
            DockFold::Held => {}
        }
        match dock_fold {
            DockFold::FirstFrame => self.fetch_heartbeat_catalog().await,
            DockFold::Fresh | DockFold::Held => self.spawn_heartbeat_refresh(),
        }
        // The slash-command catalog is session-scoped too; the skill commands
        // land in the autocomplete provider when the response arrives.
        self.spawn_command_catalog_refresh();
        // The dock's bash rows follow the same `dock_fold` contract; a failed fold
        // fetch leaves the cleared registry (the 2s poll refills).
        match dock_fold {
            DockFold::FirstFrame | DockFold::Fresh => {
                self.bash_activities = serde_json::json!({"activities": []});
                // The factory lane's cache dies with the previous
                // session too: the dock count and the page mount must
                // never show another session's runs (the 2s poll
                // refills; the spawn below asks for the first frame).
                self.factory_graph = serde_json::json!({"runs": []});
            }
            DockFold::Held => {}
        }
        self.activity_group = crate::chrome::ActivityGroup::Subagents;
        match dock_fold {
            DockFold::FirstFrame => self.fetch_bash_activities().await,
            DockFold::Fresh | DockFold::Held => self.spawn_bash_activity_refresh(),
        }
        // The refilled factory cache is a background refresh away on
        // every fold (the poll's serialization makes an immediate
        // request safe — the in-flight slot frees on its own cycle).
        self.spawn_factory_refresh();
        self.pending_model = reconstructed.model_id;
        self.pending_model_provider = reconstructed.model_provider;
        self.pending_thinking_suffix = reconstructed.thinking_suffix;
        // The tray's context usage follows the attach snapshot (TS
        // `createAgentConnectionState` carries `contextUsage`, and TS's
        // open path never blocks its first frame on a `getSessionStats`
        // fetch — `refreshConnectionContextUsage` runs only after a turn
        // or compaction settles): the rebuild below re-syncs it into the
        // chrome exactly like the other reconstructed session fields.
        self.context = reconstructed.context_usage;
        self.last_assistant_text = last_assistant_text_of(&reconstructed.chat);
        self.pending_queue = Some(reconstructed.queued);
        self.pending_snapshot = Some(reconstructed.chat);
        self.pending_streaming_message = reconstructed.streaming_assistant;
        self.loader_anchor_ms = reconstructed.last_user_prompt_ms;
        self.goal_view.seed(reconstructed.goal.unwrap_or_default());
        self.settled = None;
        self.outcome_after_compaction = None;
        // The resynced state owns the loader: a turn still live behind the
        // re-attach keeps the spinner, one that died with the old link does
        // not. A compaction in flight seeds the compaction loader instead
        // (see `rebuild_view`): its `compaction_end` clears it.
        let streaming = attach
            .snapshot
            .get("state")
            .and_then(|state| state.get("isStreaming"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.turn_active = streaming;
        self.run_open = streaming;
        self.run_outcome = crate::program_status::Status::Done;
        self.streaming_index = None;
        // The turn-end watermark restarts only when the mounted session CHANGES: an end owed by the
        // detached session's stream must not pin the watermark (without the reset a later prompt's
        // ack would re-arm `turn_active` against an end count that can never catch up); a
        // SAME-SESSION rebind keeps the counters, or in-flight acks orphan the same way.
        if mounted_session_changes {
            self.turn_ends_seen = 0;
            self.last_prompt_turn_end = 0;
        }
        // The stash state follows the stable id of the session now rendered: the initial attach and
        // every in-place switch rebind through here.
        let stash_session_id = self.session_id.clone();
        self.bind_prompt_stash_session(&stash_session_id);
        // The attach fold's wire frame and decoded tree drop here: return their freed heap to the
        // OS instead of keeping the load's peak resident for the TUI's lifetime.
        radiata_types::memory_release::trim_freed_heap();
        // The rebuild's first frame materializes the visible window; arm the
        // post-frame trim so its wrap/render churn returns too.
        self.trim_after_frame = true;
        self.window.reset(
            attach.snapshot["state"]["leafId"]
                .as_str()
                .map(str::to_owned),
            reconstructed.history_before_entry_id,
            reconstructed.backfill_seam,
        );
        Ok(())
    }

    /// The instant a rebuilt loader anchors at, from the LAST HUMAN PROMPT's wall-clock time
    /// (operator's 2026-09-28 rule: the timer never resets on a view transition). Like TS
    /// `restoreTurnStartFromMessages`, no plausibility cap: only a FUTURE timestamp keeps the
    /// re-attach-instant anchor.
    pub(super) fn loader_anchor_instant(prompt_ms: u64) -> Option<std::time::Instant> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or_default();
        let age = now_ms.checked_sub(prompt_ms)?;
        std::time::Instant::now().checked_sub(std::time::Duration::from_millis(age))
    }

    /// The one transcript replacement: clear the chat, render the entries,
    /// and re-register decoded pending tool cards.
    pub(crate) fn replace_transcript(&mut self, entries: Vec<ChatEntry>, view: &mut AgentView) {
        view.clear_chat();
        self.last_status_index = None;
        self.pressed_click = None;
        self.pending_tools.clear();
        self.aborted_tools.clear();
        for entry in entries {
            view.push_entry(entry);
        }
        for entry in &view.chat {
            if let ChatEntry::Tool(card) = entry {
                if card.result.is_none() || card.result_partial {
                    self.pending_tools.insert(card.id.clone());
                }
            }
        }
    }

    /// Fold the pending snapshot into the view (fresh transcript, footer
    /// labels). Called after attach and after every session switch.
    pub(crate) fn rebuild_view(&mut self, view: &mut AgentView, kind: &RebuildKind) {
        self.optimistic_user_echo = None;
        self.captain_lead_sent_at = None;
        let resync_bash = self.resync_bash.take();
        // The held cards' fate diverges by rebuild: a rebind drops them with the
        // old transcript, a resync keeps them mounted above the indicator.
        let held_bash = matches!(kind, RebuildKind::Resync)
            .then(|| std::mem::take(&mut view.pending_bash))
            .unwrap_or_default();
        // A rebind replaces the whole view: the previous session's open bash view
        // dies with its transcript, and the tok/sec stats restart per session.
        if matches!(kind, RebuildKind::Rebind) {
            view.close_overlay_if(|overlay| matches!(overlay, Overlay::Bash(_)));
            // The factory view dies the same death — it snapshots the
            // previous session's kernel runs, and its refresh lane must
            // not keep polling the new session's kernel for a view
            // nobody mounted — but ONLY when a different session actually
            // took the view's place. A same-session rebind (the `Unknown
            // active session` reattach in `prompt.rs` replays over the
            // same durable session) keeps the view mounted: the user
            // neither switched sessions nor closed the view, and its
            // refresh lane re-targets the reattached session's kernel on
            // the next tick.
            // The page dies only when it was mounted under a DIFFERENT
            // session: a same-session rebind (the `Unknown active session`
            // reattach) keeps the page and its count — the kernel and its
            // runs did not change. A page that was never mounted owns no
            // state here; the attach fold owns the cache's cross-session
            // reset (every rebind path attaches first, so a switch's
            // stale cache is cleared before the rebuild lands).
            if self
                .factory_view_session
                .as_deref()
                .is_some_and(|session| session != self.session_id)
                || (matches!(view.overlay, Some(Overlay::Factory(_)))
                    && self.factory_view_session.as_deref() != Some(self.session_id.as_str()))
            {
                view.close_overlay_if(|overlay| matches!(overlay, Overlay::Factory(_)));
                self.factory_selected_run = None;
                self.factory_view_session = None;
                self.factory_graph = serde_json::json!({"runs": []});
            }
            // An agents-view panel the rebind tore down takes its return-to-agents-view flag
            // with it; a kept factory page keeps it.
            self.panel_returns_to_agents_view &= matches!(view.overlay, Some(Overlay::Factory(_)));
            // The goal panel dies with the old session too: it is a
            // snapshot of the previous session's goal state, and until
            // the new session's own `goal_update` lands it would keep
            // owning the frame over the rebind with stale content. The usage panel's
            // session spend belongs to the old session as well.
            view.close_overlay_if(|overlay| {
                matches!(
                    overlay,
                    Overlay::Goal(_) | Overlay::Info(_) | Overlay::Usage(_) | Overlay::Router(_)
                )
            });
            // Preserve the browser across same-session reattach, like the factory page.
            if self
                .library_view_session
                .as_deref()
                .is_some_and(|session| session != self.session_id)
            {
                view.close_overlay_if(|overlay| matches!(overlay, Overlay::Library(_)));
                self.library_view_session = None;
                self.library_detail_epoch = self.library_detail_epoch.wrapping_add(1);
                self.library_list_epoch = self.library_list_epoch.wrapping_add(1);
            }
            self.speed_stats = None;
            view.chrome.speed_text = None;
        }
        if let Some(Overlay::Library(library)) = view.overlay.as_ref() {
            // Retry after recovery and fence replies from the previous attachment.
            let open_machine = library.open_machine_name();
            self.library_list_epoch = self.library_list_epoch.wrapping_add(1);
            self.spawn_library_list_refresh();
            // An open drill-in rides the same retry: its request carried the
            // previous wire id, so re-issue it rather than leaving the page on
            // the loader until the user backs out and drills in again.
            if let Some(name) = open_machine {
                self.library_detail_epoch = self.library_detail_epoch.wrapping_add(1);
                self.spawn_library_graph_fetch(name);
            }
        }
        // Retry countdowns belong to the previous event stream. The fresh
        // snapshot's activity flags own this attachment; only a new retry
        // event can establish a countdown for it.
        view.retry = None;
        // A compaction in flight behind the attach keeps its loader until its
        // `compaction_end`; the snapshot names no reason, so the wire's
        // fallback (`manual`) labels it.
        if let Some(compacting) = self.attach_compacting.take() {
            view.compaction = None;
            if compacting {
                view.compaction_generation += 1;
                view.compaction = Some(CompactionState {
                    reason: CompactionReason::Manual,
                    custom_instructions: None,
                    summary: String::new(),
                });
            }
        }
        let snapshot = self.pending_snapshot.take().unwrap_or_default();
        let outgoing = view.reader_position();
        let restore = view
            .chrome
            .nav
            .switch_transcript(&self.session_id, outgoing);
        self.replace_transcript(snapshot, view);
        if let Some(message) = self.pending_model_fallback_message.take() {
            view.push_entry(ChatEntry::Status {
                text: format!("\u{26a0} {message}"),
                kind: crate::chat::StatusKind::Warning,
            });
            self.last_status_index = None;
        }
        if let Some(model) = self.pending_model.take() {
            view.chrome.model_id = Some(model);
            view.chrome.model_provider = self.pending_model_provider.take();
        }
        // The tray badge mirrors the session-scoped tier on every rebuild: an
        // attach that reports no tier clears the previous session's badge.
        view.chrome.service_tier.clone_from(&self.service_tier);
        // The tray's effort suffix moves with the same snapshot; the bare name
        // wins when the state reports neither.
        view.chrome.thinking_suffix = self.pending_thinking_suffix.take();
        view.queued = self.pending_queue.take().unwrap_or_default();
        // Any browse selection belonged to the previous queue and drops.
        let _ = self.queue_selection.reset();
        view.queue_selected = None;
        view.chrome.chat_name = self.session_display();
        view.chrome.context = self.context;
        // The rebuilt transcript invalidates the announcement row tracking;
        // the goal state itself carries over (seeded at attach).
        self.goal_view.reset_row_tracking();
        self.sync_goal_tray(view);
        self.sync_activity_dock(view);
        view.pending_bash = held_bash;
        // The rebuild decides the mounted card's fate: a rebind drops it with the old transcript; a
        // resync runs the bashFinished edge — a run that ended behind the dead link settles its
        // card with an unknown exit.
        match kind {
            RebuildKind::Rebind => {
                self.user_bash_card = None;
                self.user_bash_started_at = None;
            }
            RebuildKind::Resync => {
                if let Some(resync) = resync_bash {
                    let bash_finished = resync.was_running && !resync.snap_running;
                    if bash_finished {
                        if let Some(card_id) = self.user_bash_card.take() {
                            if let Some(index) = view.chat.iter().position(|entry| {
                                matches!(entry, ChatEntry::BashExecution(card) if card.id == card_id)
                            }) {
                                if let Some(ChatEntry::BashExecution(card)) =
                                    view.chat.get_mut(index)
                                {
                                    card.set_complete(None, false, false, None);
                                }
                                view.mark_entry_stale(index);
                            } else if let Some(card) = view
                                .pending_bash
                                .iter_mut()
                                .find(|card| card.id == card_id)
                            {
                                card.set_complete(None, false, false, None);
                            }
                            self.user_bash_started_at = None;
                            // TS flushes inside the active-component branch:
                            // a side run (no mounted card) never flushes.
                            if !resync.snap_streaming {
                                Self::flush_pending_bash(view);
                            }
                        }
                        if self.side_bash.take().is_some() {
                            if let Some(pane) = view.side_pane.as_mut() {
                                if let Some(bash) = pane.bash.as_mut() {
                                    bash.running = false;
                                }
                            }
                        }
                        self.side_bash_discarded = None;
                    }
                }
            }
        }
        // An attached turn that survived the re-attach keeps its loader; the re-mounted loader
        // anchors at the LAST HUMAN PROMPT (the operator's 2026-09-28 rule).
        if self.turn_active {
            let anchor = self
                .loader_anchor_ms
                .and_then(Self::loader_anchor_instant)
                .unwrap_or_else(std::time::Instant::now);
            self.start_loader_at(view, anchor);
            if let Some(streaming_message) = self.pending_streaming_message.take() {
                self.apply_assistant_message(&streaming_message, true, None, view);
            }
        } else {
            self.pending_streaming_message = None;
            view.working = None;
        }
        view.follow();
        // A chat switched back to resumes where its reader left it.
        if let Some((entry, row)) = restore.filter(|(entry, _)| *entry < view.chat.len()) {
            view.scroll_to_entry(entry, row);
        }
        // An open `/heartbeats` picker follows the rebuilt session's catalog — otherwise a rebind
        // leaves it showing the previous session's rows.
        if let Some(Overlay::Heartbeats(picker)) = view.overlay.as_mut() {
            picker.apply_catalog(self.heartbeat_catalog.clone(), None);
        }
        // The brand splash is the EMPTY chat's header: a rebuild that folds a non-empty transcript
        // suppresses it, every rebuild into an empty chat keeps it.
        view.splash_suppressed = !view.chat.is_empty();
        self.dirty = true;
    }

    /// Refresh context usage from `get_session_stats`: tokens, context window,
    /// and percent. The title's spend comes from the roster pushes instead.
    pub(crate) async fn refresh_stats(&mut self) {
        let Ok(data) = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetSessionStats {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Map::default(),
                },
            )
            .await
        else {
            return;
        };
        // Unknown usage (tokens null right after a compaction, or a
        // response without the field) clears the tray display.
        self.context = data
            .get("contextUsage")
            .and_then(crate::chrome::ContextUsage::from_wire);
        self.dirty = true;
    }

    /// Detach on the agents-view handoff without blocking it: a background task owns the connection
    /// until the daemon answers; the supervisor also detaches this client when the socket closes.
    pub(crate) fn detach_for_handoff(&self) {
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                Duration::from_millis(EXIT_DETACH_TIMEOUT_MS),
                client.request_ok(DaemonCommand::Detach {
                    id: None,
                    active_session_id: Some(active_session_id),
                    rest: Map::default(),
                }),
            )
            .await;
            client.close();
        });
    }

    /// Detach during the exit path, bounded hard: a wedged worker socket can never hold the client
    /// open (the exit contract is exit-within-1s even then, so this must stay well under the
    /// bound).
    pub(crate) async fn detach_for_exit(&self) {
        let _ = tokio::time::timeout(
            Duration::from_millis(EXIT_DETACH_TIMEOUT_MS),
            self.client.request_ok(DaemonCommand::Detach {
                id: None,
                active_session_id: Some(self.active_session_id.clone()),
                rest: Map::default(),
            }),
        )
        .await;
    }

    /// Fetch the exit resume hint, bounded best-effort: a wedged worker or a
    /// dead connection yields no hint instead of holding the exit open.
    pub(crate) async fn exit_resume_hint(&self) -> Option<String> {
        let stats = tokio::time::timeout(
            Duration::from_millis(EXIT_STATS_TIMEOUT_MS),
            self.client.request_ok(DaemonCommand::GetSessionStats {
                id: None,
                active_session_id: self.active_session_id.clone(),
                rest: Map::default(),
            }),
        )
        .await;
        resume_hint_from_stats(&stats.ok()?.ok()?)
    }
}

/// The attached session's working directory from its snapshot (the state's
/// `cwd`, else the summary's), when it names a directory that still exists.
fn attached_session_cwd(snapshot: &Value) -> Option<PathBuf> {
    ["state", "summary"]
        .iter()
        .filter_map(|section| snapshot.get(section)?.get("cwd")?.as_str())
        .find(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .filter(|cwd| cwd.is_dir())
}

#[cfg(all(test, unix))]
mod backfill_tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// One session attached over a mock daemon that answers `attach` and `get_messages`.
    async fn mock_session(
        socket: &std::path::Path,
        backfill_tx: mpsc::UnboundedSender<TranscriptPageNote>,
    ) -> (
        SessionUi,
        tokio::task::JoinHandle<()>,
        mpsc::UnboundedReceiver<Value>,
    ) {
        let (requests_tx, requests_rx) = mpsc::unbounded_channel();
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            write.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"serverCapabilities\":[\"factory_activity\"],\"clientId\":\"mock\"}\n").await.unwrap();
            let mut lines = BufReader::new(read).lines();
            let mut attach_count = 0;
            let mut graph_replies = Vec::new();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                let kind = request["command"]["type"].as_str().unwrap();
                if kind == "factory_activity" && request["command"]["action"] == "library" {
                    requests_tx.send(request["command"].clone()).unwrap();
                }
                let data = match kind {
                    "factory_activity" if request["command"]["specId"].is_string() => {
                        json!({"specId": request["command"]["specId"], "machine": {}})
                    }
                    "factory_activity" => json!({"machines": [
                        {"name": "first", "description": "first machine", "source": "repo"},
                        {"name": "second", "description": "second machine", "source": "repo"},
                    ]}),
                    "attach" => {
                        attach_count += 1;
                        let active_id = format!("active-{attach_count}");
                        json!({
                            "activeSessionId": active_id, "snapshot": {
                                "activeSessionId": active_id, "summary": {"id": "s1", "cwd": "/tmp"},
                                "state": {"sessionId": "s1", "cwd": "/tmp", "isStreaming": false},
                                "messages": [], "historyBefore": 1, "lastEventSequence": 0,
                            }, "replay": {"status": "complete", "toSequence": 0},
                            "lastEventSequence": 0,
                        })
                    }
                    "get_messages" => {
                        json!({"messages": [{"role": "user", "content": "rebuilt transcript", "timestamp": 1}]})
                    }
                    _ => json!({}),
                };
                let mut reply = serde_json::to_vec(&json!({
                    "type": "response", "id": request["id"], "command": kind,
                    "success": true, "data": data,
                }))
                .unwrap();
                reply.push(b'\n');
                if kind == "factory_activity" && request["command"]["specId"].is_string() {
                    // Hold graph replies until a reattach, without holding other requests.
                    graph_replies.push(reply);
                    continue;
                }
                write.write_all(&reply).await.unwrap();
                if kind == "attach" {
                    for graph_reply in graph_replies.drain(..) {
                        write.write_all(&graph_reply).await.unwrap();
                    }
                }
            }
        });
        let mut options =
            crate::interactive::tests::options(crate::interactive::ModelSelection::default());
        options.socket_path = socket.to_path_buf();
        options.session = SessionSelection::Attach("s1".to_string());
        let (client, _events) = DaemonClient::connect(socket).await.unwrap();
        let session = SessionUi::open(
            client,
            &options,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            ActivityUpdates {
                heartbeats: mpsc::unbounded_channel().0,
                bash: mpsc::unbounded_channel().0,
                factory: mpsc::unbounded_channel().0,
                library: mpsc::unbounded_channel().0,
                commands: mpsc::unbounded_channel().0,
                captain: mpsc::unbounded_channel().0,
                usage: mpsc::unbounded_channel().0,
            },
            backfill_tx,
        )
        .await
        .unwrap();
        (session, server, requests_rx)
    }

    fn library_view(view: &AgentView) -> Option<&crate::factory_view::LibraryView> {
        match view.overlay.as_ref() {
            Some(Overlay::Library(library)) => Some(library),
            _ => None,
        }
    }

    fn library_view_mut(view: &mut AgentView) -> Option<&mut crate::factory_view::LibraryView> {
        match view.overlay.as_mut() {
            Some(Overlay::Library(library)) => Some(library),
            _ => None,
        }
    }

    #[tokio::test]
    async fn library_rebind_preserves_only_the_same_durable_session() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, mut requests) = mock_session(
                &dir.path().join("library-rebind.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            session.open_library_page(&mut view);
            assert_eq!(requests.recv().await.unwrap()["action"], "library");
            library_view_mut(&mut view).unwrap().apply_machines(
                crate::factory_view::parse_library_machines(&json!({"machines": [
                    {"name": "first", "description": "", "source": "repo"},
                    {"name": "second", "description": "", "source": "repo"},
                ]})),
            );
            let down = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            );
            session
                .handle_key(down, &mut view, &mut true)
                .await
                .unwrap();
            session.attach_session("s1", DockFold::Held).await.unwrap();
            session.rebuild_view(&mut view, &RebuildKind::Rebind);
            assert_eq!(
                requests.recv().await.unwrap()["action"],
                "library",
                "reattach retargets the list fetch"
            );
            assert_eq!(
                library_view(&view)
                    .unwrap()
                    .selected_machine()
                    .unwrap()
                    .name,
                "second"
            );
            session.session_id = "different-session".to_string();
            session.rebuild_view(&mut view, &RebuildKind::Rebind);
            assert!(library_view(&view).is_none());
            assert!(session.library_view_session.is_none());
            drop(session);
            server.abort();
        })
        .await
        .expect("library session path completes within ten seconds");
    }

    /// One Ctrl+C press dispatched at the library page counts once for the
    /// force-quit guard: the session loop already reports every dispatched
    /// key, so the page must not report the same press again. A second
    /// count would disarm the watchdog while a queued press is unhandled.
    #[tokio::test]
    async fn one_library_ctrl_c_press_reports_one_handled_key() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, _requests) = mock_session(
                &dir.path().join("library-ctrl-c.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            view.overlay = Some(Overlay::Library(crate::factory_view::LibraryView::new(
                Vec::new(),
                20,
            )));
            session
                .handle_key(
                    crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Char('c'),
                        crossterm::event::KeyModifiers::CONTROL,
                    ),
                    &mut view,
                    &mut true,
                )
                .await
                .unwrap();
            assert_eq!(
                session.exit_guard.handled_ctrl_c(),
                1,
                "one dispatched Ctrl+C is one handled press, whichever surface consumes it"
            );
            assert!(library_view(&view).is_none(), "the library page closes");
            drop(session);
            server.abort();
        })
        .await
        .expect("library Ctrl+C dispatch completes within ten seconds");
    }

    #[tokio::test]
    async fn library_graph_reply_survives_same_session_rebind() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, mut requests) = mock_session(
                &dir.path().join("library-rebind-graph.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
            session.library_updates = updates_tx;
            session.open_library_page(&mut view);
            assert_eq!(requests.recv().await.unwrap()["action"], "library");
            session.apply_library_update(updates_rx.recv().await.unwrap(), &mut view);
            session
                .handle_key(
                    crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Enter,
                        crossterm::event::KeyModifiers::NONE,
                    ),
                    &mut view,
                    &mut true,
                )
                .await
                .unwrap();
            let graph_request = requests.recv().await.unwrap();
            assert_eq!(graph_request["specId"], "first");
            assert_eq!(graph_request["activeSessionId"], session.active_session_id);
            let previous_active_id = session.active_session_id.clone();
            let durable_id = session.session_id.clone();
            session
                .attach_session(&durable_id, DockFold::Held)
                .await
                .unwrap();
            assert_ne!(session.active_session_id, previous_active_id);
            assert_eq!(session.session_id, durable_id);
            session.rebuild_view(&mut view, &RebuildKind::Rebind);
            let (graph_session, graph_epoch) = loop {
                let update = updates_rx.recv().await.unwrap();
                let graph_identity = match &update {
                    crate::session_ui::LibraryUpdate::Graph { session, epoch, .. } => {
                        Some((session.clone(), *epoch))
                    }
                    crate::session_ui::LibraryUpdate::Machines { .. }
                    | crate::session_ui::LibraryUpdate::Error { .. } => None,
                };
                session.apply_library_update(update, &mut view);
                if let Some(identity) = graph_identity {
                    break identity;
                }
            };
            let frame = view.render_frame(100, 30);
            let text = frame
                .iter()
                .flatten()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(
                text.contains("machine: first"),
                "the old attachment's pending graph opens after same-session rebind: {text}"
            );
            session.apply_library_update(
                crate::session_ui::LibraryUpdate::Graph {
                    session: graph_session.clone(),
                    epoch: graph_epoch,
                    name: "first".to_string(),
                    result: Err("graph request failed before reattach".to_string()),
                },
                &mut view,
            );
            let frame = view.render_frame(100, 30);
            let text = frame
                .iter()
                .flatten()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(
                text.contains("graph request failed before reattach"),
                "{text}"
            );
            session.session_id = "different-session".to_string();
            session.rebuild_view(&mut view, &RebuildKind::Rebind);
            session.open_library_page(&mut view);
            session.dirty = false;
            session.apply_library_update(
                crate::session_ui::LibraryUpdate::Graph {
                    session: graph_session,
                    epoch: session.library_detail_epoch,
                    name: "first".to_string(),
                    result: Ok(json!({"specId": "first", "machine": {}})),
                },
                &mut view,
            );
            assert!(
                !session.dirty,
                "a different durable session rejects the graph"
            );
            drop(session);
            server.abort();
        })
        .await
        .expect("library graph survives reattachment within ten seconds");
    }

    #[tokio::test]
    async fn library_resync_retries_the_list_and_rejects_previous_replies() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, mut requests) = mock_session(
                &dir.path().join("library-resync.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
            session.library_updates = updates_tx;
            session.open_library_page(&mut view);
            assert_eq!(requests.recv().await.unwrap()["action"], "library");
            let previous_epoch = session.library_list_epoch;
            let previous_reply = updates_rx.recv().await.unwrap();
            session.apply_library_update(
                crate::session_ui::LibraryUpdate::Error {
                    session: session.active_session_id.clone(),
                    epoch: previous_epoch,
                    message: "disconnected list request".to_string(),
                },
                &mut view,
            );
            session.attach_session("s1", DockFold::Held).await.unwrap();
            session.rebuild_view(&mut view, &RebuildKind::Resync);
            assert_eq!(requests.recv().await.unwrap()["action"], "library");
            session.apply_library_update(previous_reply, &mut view);
            assert!(
                library_view(&view).unwrap().selected_machine().is_none(),
                "the previous attachment's list cannot populate the page"
            );
            let fresh_reply = updates_rx.recv().await.unwrap();
            session.apply_library_update(fresh_reply, &mut view);
            assert_eq!(
                library_view(&view)
                    .unwrap()
                    .selected_machine()
                    .unwrap()
                    .name,
                "first"
            );
            session.apply_library_update(
                crate::session_ui::LibraryUpdate::Error {
                    session: session.active_session_id.clone(),
                    epoch: previous_epoch,
                    message: "late disconnected error".to_string(),
                },
                &mut view,
            );
            let frame = view.render_frame(100, 30);
            let text = frame
                .iter()
                .flatten()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(!text.contains("disconnected"), "{text}");
            drop(session);
            server.abort();
        })
        .await
        .expect("library recovery retries the list within ten seconds");
    }

    #[tokio::test]
    async fn library_graph_survives_harmless_keys_but_not_selection_changes() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, mut requests) = mock_session(
                &dir.path().join("library-harmless-keys.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            let machines = crate::factory_view::parse_library_machines(&json!({"machines": [
                {"name": "first", "description": "", "source": "repo"},
            ]}));
            for key in [KeyCode::Char('x'), KeyCode::Up, KeyCode::Down] {
                for result in [
                    Ok(json!({"specId": "first", "machine": {}})),
                    Err("graph request failed".to_string()),
                ] {
                    view.overlay = Some(Overlay::Library(crate::factory_view::LibraryView::new(
                        machines.clone(),
                        20,
                    )));
                    session
                        .handle_library_view_key(
                            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                            &mut view,
                        )
                        .unwrap();
                    assert_eq!(requests.recv().await.unwrap()["specId"], "first");
                    let epoch = session.library_detail_epoch;
                    session
                        .handle_key(KeyEvent::new(key, KeyModifiers::NONE), &mut view, &mut true)
                        .await
                        .unwrap();
                    let expected = if result.is_ok() {
                        "machine: first"
                    } else {
                        "graph request failed"
                    };
                    session.apply_library_update(
                        crate::session_ui::LibraryUpdate::Graph {
                            session: session.session_id.clone(),
                            epoch,
                            name: "first".to_string(),
                            result,
                        },
                        &mut view,
                    );
                    let frame = view.render_frame(100, 30);
                    let text = frame
                        .iter()
                        .flatten()
                        .map(|span| span.content.as_str())
                        .collect::<String>();
                    assert!(
                        text.contains(expected),
                        "{key:?} must preserve the pending reply: {text}"
                    );
                }
            }
            view.overlay = Some(Overlay::Library(crate::factory_view::LibraryView::new(
                crate::factory_view::parse_library_machines(&json!({"machines": [
                    {"name": "first", "description": "", "source": "repo"},
                    {"name": "second", "description": "", "source": "repo"},
                ]})),
                20,
            )));
            session
                .handle_library_view_key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    &mut view,
                )
                .unwrap();
            assert_eq!(requests.recv().await.unwrap()["specId"], "first");
            let epoch = session.library_detail_epoch;
            session
                .handle_key(
                    KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    &mut view,
                    &mut true,
                )
                .await
                .unwrap();
            assert_eq!(
                library_view(&view)
                    .unwrap()
                    .selected_machine()
                    .unwrap()
                    .name,
                "second"
            );
            for result in [
                Ok(json!({"specId": "first", "machine": {}})),
                Err("stale graph failure".to_string()),
            ] {
                session.dirty = false;
                session.apply_library_update(
                    crate::session_ui::LibraryUpdate::Graph {
                        session: session.session_id.clone(),
                        epoch,
                        name: "first".to_string(),
                        result,
                    },
                    &mut view,
                );
                assert!(
                    !session.dirty,
                    "changed selection rejects the previous graph result or error"
                );
            }
            let frame = view.render_frame(100, 30);
            let text = frame
                .iter()
                .flatten()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(!text.contains("machine: first"), "{text}");
            assert!(!text.contains("stale graph failure"), "{text}");
            drop(session);
            server.abort();
        })
        .await
        .expect("library graph keys complete within ten seconds");
    }

    #[tokio::test]
    async fn library_list_refresh_retires_graph_only_when_selected_machine_changes() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, mut requests) = mock_session(
                &dir.path().join("library-refresh-selection.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            let first = json!({"name": "first", "description": "", "source": "repo"});
            let second = json!({"name": "second", "description": "", "source": "repo"});
            for remove_selected in [true, false] {
                for result in [
                    Ok(json!({"specId": "first", "machine": {}})),
                    Err("graph request failed".to_string()),
                ] {
                    view.overlay = Some(Overlay::Library(crate::factory_view::LibraryView::new(
                        crate::factory_view::parse_library_machines(&json!({
                            "machines": [first.clone(), second.clone()]
                        })),
                        20,
                    )));
                    session
                        .handle_library_view_key(
                            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                            &mut view,
                        )
                        .unwrap();
                    assert_eq!(requests.recv().await.unwrap()["specId"], "first");
                    let epoch = session.library_detail_epoch;
                    let refreshed = if remove_selected {
                        vec![second.clone()]
                    } else {
                        vec![second.clone(), first.clone()]
                    };
                    session.apply_library_update(
                        crate::session_ui::LibraryUpdate::Machines {
                            session: session.active_session_id.clone(),
                            epoch: session.library_list_epoch,
                            data: json!({"machines": refreshed}),
                        },
                        &mut view,
                    );
                    assert_eq!(
                        library_view(&view)
                            .unwrap()
                            .selected_machine()
                            .unwrap()
                            .name,
                        if remove_selected { "second" } else { "first" }
                    );
                    let reply_text = if result.is_ok() {
                        "machine: first"
                    } else {
                        "graph request failed"
                    };
                    session.apply_library_update(
                        crate::session_ui::LibraryUpdate::Graph {
                            session: session.session_id.clone(),
                            epoch,
                            name: "first".to_string(),
                            result,
                        },
                        &mut view,
                    );
                    let frame = view.render_frame(100, 30);
                    let text = frame
                        .iter()
                        .flatten()
                        .map(|span| span.content.as_str())
                        .collect::<String>();
                    assert_eq!(
                        text.contains(reply_text),
                        !remove_selected,
                        "list refresh must retire replies only when selection changes: {text}"
                    );
                }
            }
            drop(session);
            server.abort();
        })
        .await
        .expect("library list selection refresh completes within ten seconds");
    }

    #[tokio::test]
    async fn library_list_success_does_not_clear_a_graph_failure() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, _requests) = mock_session(
                &dir.path().join("library-error-order.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            let machines = json!({"machines": [
                {"name": "first", "description": "", "source": "repo"}
            ]});
            view.overlay = Some(Overlay::Library(crate::factory_view::LibraryView::new(
                crate::factory_view::parse_library_machines(&machines),
                20,
            )));
            for graph_failed in [false, true] {
                session.apply_library_update(
                    crate::session_ui::LibraryUpdate::Error {
                        session: session.active_session_id.clone(),
                        epoch: session.library_list_epoch,
                        message: "list request failed".to_string(),
                    },
                    &mut view,
                );
                if graph_failed {
                    session.apply_library_update(
                        crate::session_ui::LibraryUpdate::Graph {
                            session: session.session_id.clone(),
                            epoch: session.library_detail_epoch,
                            name: "first".to_string(),
                            result: Err("graph request failed".to_string()),
                        },
                        &mut view,
                    );
                }
                session.apply_library_update(
                    crate::session_ui::LibraryUpdate::Machines {
                        session: session.active_session_id.clone(),
                        epoch: session.library_list_epoch,
                        data: machines.clone(),
                    },
                    &mut view,
                );
                let frame = view.render_frame(100, 30);
                let text = frame
                    .iter()
                    .flatten()
                    .map(|span| span.content.as_str())
                    .collect::<String>();
                assert!(!text.contains("list request failed"), "{text}");
                assert_eq!(
                    text.contains("graph request failed"),
                    graph_failed,
                    "{text}"
                );
            }
            session.apply_library_update(
                crate::session_ui::LibraryUpdate::Graph {
                    session: session.session_id.clone(),
                    epoch: session.library_detail_epoch,
                    name: "first".to_string(),
                    result: Ok(json!({"specId": "first", "machine": {}})),
                },
                &mut view,
            );
            let frame = view.render_frame(100, 30);
            let text = frame
                .iter()
                .flatten()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(text.contains("machine: first"), "{text}");
            assert!(!text.contains("graph request failed"), "{text}");
            for replace_list in [false, true] {
                let two_machines = json!({"machines": [
                    {"name": "first", "description": "", "source": "repo"},
                    {"name": "second", "description": "", "source": "repo"}
                ]});
                view.overlay = Some(Overlay::Library(crate::factory_view::LibraryView::new(
                    crate::factory_view::parse_library_machines(&two_machines),
                    20,
                )));
                session.apply_library_update(
                    crate::session_ui::LibraryUpdate::Graph {
                        session: session.session_id.clone(),
                        epoch: session.library_detail_epoch,
                        name: "first".to_string(),
                        result: Err("graph request failed".to_string()),
                    },
                    &mut view,
                );
                if replace_list {
                    session.apply_library_update(
                        crate::session_ui::LibraryUpdate::Machines {
                            session: session.active_session_id.clone(),
                            epoch: session.library_list_epoch,
                            data: json!({"machines": [two_machines["machines"][1].clone()]}),
                        },
                        &mut view,
                    );
                } else {
                    session
                        .handle_library_view_key(
                            crossterm::event::KeyEvent::new(
                                crossterm::event::KeyCode::Down,
                                crossterm::event::KeyModifiers::NONE,
                            ),
                            &mut view,
                        )
                        .unwrap();
                }
                assert_eq!(
                    library_view(&view)
                        .unwrap()
                        .selected_machine()
                        .unwrap()
                        .name,
                    "second"
                );
                let frame = view.render_frame(100, 30);
                let text = frame
                    .iter()
                    .flatten()
                    .map(|span| span.content.as_str())
                    .collect::<String>();
                assert!(!text.contains("graph request failed"), "{text}");
            }
            drop(session);
            server.abort();
        })
        .await
        .expect("library error ordering completes within ten seconds");
    }

    #[tokio::test]
    async fn library_navigation_dirties_selection_and_detail_back() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, _requests) = mock_session(
                &dir.path().join("library-keys.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            session.open_library_page(&mut view);
            library_view_mut(&mut view).unwrap().apply_machines(
                crate::factory_view::parse_library_machines(&json!({"machines": [
                    {"name": "first", "description": "", "source": "repo"},
                    {"name": "second", "description": "", "source": "repo"},
                ]})),
            );
            session.dirty = false;
            session
                .handle_key(
                    crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Down,
                        crossterm::event::KeyModifiers::NONE,
                    ),
                    &mut view,
                    &mut true,
                )
                .await
                .unwrap();
            assert!(
                session.dirty,
                "selection navigation repaints without another event"
            );
            assert_eq!(
                library_view(&view)
                    .unwrap()
                    .selected_machine()
                    .unwrap()
                    .name,
                "second"
            );
            library_view_mut(&mut view)
                .unwrap()
                .apply_detail("second", &json!({"specId": "second", "machine": {}}));
            session.dirty = false;
            session
                .handle_key(
                    crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Esc,
                        crossterm::event::KeyModifiers::NONE,
                    ),
                    &mut view,
                    &mut true,
                )
                .await
                .unwrap();
            assert!(session.dirty, "back from detail repaints the list");
            let frame = view.render_frame(100, 30);
            let text = frame
                .iter()
                .flatten()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(text.contains("machine library"), "{text}");
            assert!(!text.contains("machine: second"), "{text}");
            drop(session);
            server.abort();
        })
        .await
        .expect("library session path completes within ten seconds");
    }

    #[tokio::test]
    async fn library_back_fences_the_pending_drill_in() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, _requests) = mock_session(
                &dir.path().join("library-back.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            session.open_library_page(&mut view);
            library_view_mut(&mut view).unwrap().apply_machines(
                crate::factory_view::parse_library_machines(&json!({"machines": [
                    {"name": "first", "description": "", "source": "repo"},
                ]})),
            );
            library_view_mut(&mut view)
                .unwrap()
                .apply_detail("first", &json!({"specId": "first", "machine": {}}));
            let epoch_before_back = session.library_detail_epoch;
            let issued_at = session.library_detail_epoch;
            session
                .handle_key(
                    crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Esc,
                        crossterm::event::KeyModifiers::NONE,
                    ),
                    &mut view,
                    &mut true,
                )
                .await
                .unwrap();
            assert!(
                session.library_detail_epoch > epoch_before_back,
                "back from the drill-in bumps the detail epoch"
            );
            // A reply issued before the back is stale and must not restore
            // the detail or its error.
            session.dirty = false;
            session.apply_library_update(
                crate::session_ui::LibraryUpdate::Graph {
                    session: session.session_id.clone(),
                    epoch: issued_at,
                    name: "first".to_string(),
                    result: Ok(json!({"specId": "first", "machine": {}})),
                },
                &mut view,
            );
            assert!(!session.dirty, "a reply from before the back is dropped");
            let frame = view.render_frame(100, 30);
            let text = frame
                .iter()
                .flatten()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(!text.contains("machine: first"), "{text}");
            drop(session);
            server.abort();
        })
        .await
        .expect("the back fence completes within ten seconds");
    }

    #[tokio::test]
    async fn library_graph_never_blocks_keys_or_reopens_a_replaced_page() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, mut requests) = mock_session(
                &dir.path().join("library-graph.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
            session.library_updates = updates_tx;
            session.open_library_page(&mut view);
            let frame = view.render_frame(100, 30);
            let text = frame
                .iter()
                .flatten()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(
                text.contains("Loading machines"),
                "the open path mounts a loader: {text}"
            );
            let update = updates_rx.recv().await.unwrap();
            session.apply_library_update(update, &mut view);
            assert_eq!(requests.recv().await.unwrap()["action"], "library");
            let enter = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            );
            tokio::time::timeout(
                Duration::from_millis(100),
                session.handle_key(enter, &mut view, &mut true),
            )
            .await
            .expect("a hanging graph request cannot hold the input path")
            .unwrap();
            assert_eq!(requests.recv().await.unwrap()["specId"], "first");
            let epoch = session.library_detail_epoch;
            let stale = |session_id: String| crate::session_ui::LibraryUpdate::Graph {
                session: session_id,
                epoch,
                name: "first".to_string(),
                result: Ok(json!({"specId": "first", "machine": {}})),
            };
            session
                .handle_key(
                    crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Down,
                        crossterm::event::KeyModifiers::NONE,
                    ),
                    &mut view,
                    &mut true,
                )
                .await
                .unwrap();
            session.dirty = false;
            session.apply_library_update(stale(session.session_id.clone()), &mut view);
            assert!(
                !session.dirty,
                "selection navigation retires the previous drill-in"
            );
            session
                .handle_key(
                    crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Esc,
                        crossterm::event::KeyModifiers::NONE,
                    ),
                    &mut view,
                    &mut true,
                )
                .await
                .unwrap();
            assert!(
                library_view(&view).is_none(),
                "Esc closes while the sink still hangs"
            );
            session.open_library_page(&mut view);
            session.dirty = false;
            session.apply_library_update(stale(session.session_id.clone()), &mut view);
            assert!(!session.dirty, "a previous mount cannot reopen its detail");
            session.apply_library_update(
                crate::session_ui::LibraryUpdate::Graph {
                    session: "old-active-session".to_string(),
                    epoch: session.library_detail_epoch,
                    name: "first".to_string(),
                    result: Ok(json!({"specId": "first", "machine": {}})),
                },
                &mut view,
            );
            assert!(
                !session.dirty,
                "a foreign active session cannot deliver a graph"
            );
            session.apply_library_update(
                crate::session_ui::LibraryUpdate::Graph {
                    session: session.session_id.clone(),
                    epoch: session.library_detail_epoch,
                    name: "first".to_string(),
                    result: Ok(json!({"specId": "first", "machine": {}})),
                },
                &mut view,
            );
            let frame = view.render_frame(100, 30);
            let text = frame
                .iter()
                .flatten()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(
                text.contains("machine: first"),
                "a current reply mounts detail: {text}"
            );
            drop(session);
            server.abort();
        })
        .await
        .expect("library session path completes within ten seconds");
    }

    #[tokio::test]
    async fn dock_library_and_factory_replace_each_other_and_consume_paste() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, _requests) = mock_session(
                &dir.path().join("library-pages.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            session.open_dock_group_from_click(crate::chrome::ActivityGroup::Factory, &mut view);
            assert!(matches!(view.overlay, Some(Overlay::Factory(_))));
            session.panel_returns_to_agents_view = true;
            session.open_dock_group_from_click(crate::chrome::ActivityGroup::Library, &mut view);
            assert!(matches!(view.overlay, Some(Overlay::Library(_))));
            assert!(!session.panel_returns_to_agents_view);
            view.editor.set_text("draft");
            assert!(view.route_paste("unwanted hidden input"));
            assert_eq!(view.editor.get_lines().join("\n"), "draft");
            session.open_dock_group_from_click(crate::chrome::ActivityGroup::Factory, &mut view);
            assert!(matches!(view.overlay, Some(Overlay::Factory(_))));
            session.captain_dock = Some(crate::chrome::CaptainDockState {
                project_id: "proj-1".into(),
                name: "alpha".into(),
                state: "active".into(),
                working: 1,
                ..Default::default()
            });
            session.open_dock_group_from_click(crate::chrome::ActivityGroup::Captain, &mut view);
            assert!(matches!(view.overlay, Some(Overlay::Captain(_))));
            assert_eq!(session.captain_focus_query.as_deref(), Some("proj-1"));
            drop(session);
            server.abort();
        })
        .await
        .expect("library session path completes within ten seconds");
    }

    /// A rebind that tears down an agents-view panel drops its return: a later dock-opened shells
    /// panel closes back to the dock, never to the agents view.
    #[tokio::test]
    async fn a_rebind_drops_the_agents_view_panel_return() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("panel.sock");
            let (mut session, server, _requests) =
                mock_session(&socket, mpsc::unbounded_channel().0).await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            session.open_agents_view_panel(
                crate::interactive::SessionPanel::Shells { shell_id: None },
                true,
                &mut view,
            );
            session.rebuild_view(&mut view, &RebuildKind::Rebind);
            assert!(
                !matches!(view.overlay, Some(Overlay::Bash(_))),
                "the rebind tore the panel down"
            );
            session.open_bash_view(&mut view);
            let esc = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            );
            session.handle_bash_view_key(esc, &mut view).unwrap();
            assert!(
                !matches!(view.overlay, Some(Overlay::Bash(_))),
                "Esc closed the dock's panel"
            );
            assert!(
                !session.open_agents_view && !session.exit_requested,
                "the dock's panel closes to the dock"
            );
            drop(session);
            server.abort();
        })
        .await
        .expect("mock session open, rebind and panel close complete within ten seconds");
    }

    /// An agents-view shell open selects its shell in the Bash panel, also when the shell lands in
    /// a later poll than the panel's first (empty) snapshot.
    #[tokio::test]
    async fn an_agents_view_shell_open_selects_its_shell() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("shell.sock");
            let (mut session, server, _requests) =
                mock_session(&socket, mpsc::unbounded_channel().0).await;
            let shells = json!({"activities": [
                {"id": "a", "command": "cargo build", "status": "running"},
                {"id": "s2", "command": "sleep 600", "status": "running"},
            ]});
            for preloaded in [true, false] {
                let theme =
                    || crate::theme::Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
                let mut view = AgentView::new(theme());
                session.bash_activities = if preloaded {
                    shells.clone()
                } else {
                    json!({"activities": []})
                };
                session.open_agents_view_panel(
                    crate::interactive::SessionPanel::Shells {
                        shell_id: Some("s2".to_string()),
                    },
                    true,
                    &mut view,
                );
                if !preloaded {
                    session.apply_bash_activity(
                        crate::session_ui::bash::BashActivityUpdate::List {
                            session: session.active_session_id.clone(),
                            epoch: session.bash_list_epoch,
                            data: shells.clone(),
                        },
                        &mut view,
                    );
                }
                let Some(Overlay::Bash(bash_view)) = &view.overlay else {
                    panic!("the Bash panel is open");
                };
                let lines = bash_view.render(&theme(), 80, view.editor.keybindings());
                let selected = lines
                    .iter()
                    .map(|line| {
                        line.iter()
                            .map(|span| span.content.as_str())
                            .collect::<String>()
                    })
                    .find(|text| text.starts_with('\u{203a}'))
                    .expect("a selected row");
                assert!(
                    selected.contains("sleep 600"),
                    "preloaded {preloaded}: {selected}"
                );
            }
            drop(session);
            server.abort();
        })
        .await
        .expect("mock session open and both panel opens complete within ten seconds");
    }

    /// A reload outcome closes only the reload box: the settings menu that started the reload stays
    /// open.
    #[tokio::test]
    async fn a_reload_outcome_leaves_the_settings_menu_open() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("reload.sock");
            let (mut session, server, _requests) =
                mock_session(&socket, mpsc::unbounded_channel().0).await;
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            session.open_settings_menu(&mut view).await;
            session
                .apply_reload_outcome(Err("reload failed".to_string()), &mut view)
                .await;
            assert!(matches!(view.overlay, Some(Overlay::Settings(_))));
            drop(session);
            server.abort();
        })
        .await
        .expect("mock session open and reload outcome complete within ten seconds");
    }

    /// Every successful attach clears an expired re-attach window's failure: the flag is
    /// per-connection, so a recovered connection must not keep refusing submits.
    #[tokio::test]
    async fn an_attach_clears_the_reconnection_failure() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("reconnect.sock");
            let (mut session, server, _requests) =
                mock_session(&socket, mpsc::unbounded_channel().0).await;
            session.connection = crate::interactive::reconnect::Connection::SessionPlaneFailed(
                "the session worker is gone".to_string(),
            );
            session.attach_session("s1", DockFold::Held).await.unwrap();
            assert!(matches!(
                session.connection,
                crate::interactive::reconnect::Connection::Connected
            ));
            drop(session);
            server.abort();
        })
        .await
        .expect("mock session open and re-attach complete within ten seconds");
    }

    #[tokio::test]
    async fn an_attach_summary_model_fallback_warns_at_the_session_open() {
        tokio::time::timeout(Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fallback.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            write.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"serverCapabilities\":[],\"clientId\":\"mock\"}\n").await.unwrap();
            let mut lines = BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                let kind = request["command"]["type"].as_str().unwrap();
                let data = match kind {
                    "attach" => json!({
                        "activeSessionId": "s1", "snapshot": {
                            "activeSessionId": "s1",
                            "summary": {
                                "id": "s1", "cwd": "/tmp",
                                "modelFallbackMessage":
                                    "Default model prime-inference/internal/glm-5.3-fast is not available. Using prime-inference/z-ai/glm-5.3",
                            },
                            "state": {"sessionId": "s1", "cwd": "/tmp", "isStreaming": false},
                            "messages": [], "lastEventSequence": 0,
                        }, "replay": {"status": "complete", "toSequence": 0},
                        "lastEventSequence": 0,
                    }),
                    _ => json!({}),
                };
                let mut reply = serde_json::to_vec(&json!({
                    "type": "response", "id": request["id"], "command": kind,
                    "success": true, "data": data,
                }))
                .unwrap();
                reply.push(b'\n');
                write.write_all(&reply).await.unwrap();
            }
        });
        let mut options =
            crate::interactive::tests::options(crate::interactive::ModelSelection::default());
        options.socket_path = socket.clone();
        options.session = SessionSelection::Attach("s1".to_string());
        let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
        let mut session = SessionUi::open(
            client,
            &options,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            ActivityUpdates {
                heartbeats: mpsc::unbounded_channel().0,
                bash: mpsc::unbounded_channel().0,
                factory: mpsc::unbounded_channel().0,
                library: mpsc::unbounded_channel().0,
                commands: mpsc::unbounded_channel().0,
                captain: mpsc::unbounded_channel().0,
                usage: mpsc::unbounded_channel().0,
            },
            mpsc::unbounded_channel().0,
        )
        .await
        .unwrap();
        let mut view = AgentView::new(crate::theme::Theme::builtin(
            "prime",
            crate::theme::ColorMode::TrueColor,
        ));
        session.rebuild_view(&mut view, &RebuildKind::Rebind);
        let warnings: Vec<String> = view
            .chat
            .iter()
            .filter_map(|entry| match entry {
                ChatEntry::Status { text, kind } if *kind == crate::chat::StatusKind::Warning => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            warnings,
            vec!["\u{26a0} Default model prime-inference/internal/glm-5.3-fast is not available. Using prime-inference/z-ai/glm-5.3".to_string()],
            "the startup fallback warns once at the open"
        );
        session.rebuild_view(&mut view, &RebuildKind::Resync);
        assert!(
            view.chat
                .iter()
                .all(|entry| !matches!(entry, ChatEntry::Status { kind: crate::chat::StatusKind::Warning, .. })),
            "a second rebuild adds no second warning"
        );
        drop(session);
        server.abort();
        }).await.expect("mock session open, warning rebuild complete within ten seconds");
    }

    fn catalog_model(provider: &str, id: &str) -> radiata_types::ai::Model {
        serde_json::from_value(json!({
            "id": id, "name": id, "api": "openai-completions", "provider": provider,
            "baseUrl": "https://example.invalid/v1", "reasoning": true,
            "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 128_000, "maxTokens": 4096,
        }))
        .unwrap()
    }

    /// Captain flows open the same `Overlay::ModelPicker` as `/model`, fed by the session's
    /// registry catalog; Esc returns to the Captain panel and Enter routes the choice back to it.
    #[tokio::test]
    async fn captain_model_choices_reuse_the_shared_model_picker() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, _requests) = mock_session(
                &dir.path().join("captain-picker.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            session.model_catalog = vec![
                catalog_model("anthropic", "claude-opus-5-5"),
                catalog_model("openai-codex", "gpt-5.4"),
            ];
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            view.overlay = Some(Overlay::Captain(crate::captain_view::CaptainView::new(20)));
            session.run_captain_action(
                crate::captain_view::CaptainAction::PickModel(
                    crate::captain_view::CaptainModelTarget::Crew {
                        project: "p1".into(),
                        member: "c1".into(),
                        label: "parser-fix".into(),
                        current_model: "openai-codex/gpt-5.4".into(),
                        current_thinking: "high".into(),
                    },
                ),
                &mut view,
            );
            let Some(Overlay::ModelPicker(picker)) = view.overlay.as_ref() else {
                panic!("the crew model choice opens the shared model picker");
            };
            assert_eq!(
                picker.filtered_len(),
                2,
                "the picker lists the registry catalog"
            );
            assert_eq!(
                picker.selected_model().map(|m| m.id.as_str()),
                Some("gpt-5.4"),
                "the crew's current model is preselected"
            );
            assert!(
                matches!(view.modal_backdrop.as_deref(), Some(Overlay::Captain(_))),
                "the panel floats dimmed behind the picker"
            );
            assert!(session.captain_parked.is_none());
            session.apply_captain_update(
                super::super::captain::CaptainUpdate::Acted {
                    op: "edit".into(),
                    done: "Background refresh landed.".into(),
                    requested_root: None,
                    result: Ok(json!({})),
                },
                &mut view,
            );
            assert!(
                matches!(view.overlay, Some(Overlay::ModelPicker(_))),
                "a background Captain update leaves the picker open"
            );
            let esc = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            );
            session
                .handle_model_picker_key(esc, &mut view)
                .await
                .unwrap();
            assert!(
                matches!(view.overlay, Some(Overlay::Captain(_))),
                "Esc returns to Captain"
            );
            assert!(view.modal_backdrop.is_none());
            assert!(matches!(
                session.model_pick_target,
                super::super::model_picker::ModelPickTarget::Chat
            ));

            // Crew assign: Enter applies the picked model to the panel's access step.
            session.run_captain_action(
                crate::captain_view::CaptainAction::PickModel(
                    crate::captain_view::CaptainModelTarget::Assign {
                        project: "p1".into(),
                    },
                ),
                &mut view,
            );
            assert!(matches!(view.overlay, Some(Overlay::ModelPicker(_))));
            let enter = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            );
            session
                .handle_model_picker_key(enter, &mut view)
                .await
                .unwrap();
            let Some(Overlay::Captain(panel)) = view.overlay.as_mut() else {
                panic!("Enter returns to Captain");
            };
            let kb = crate::keybindings::KeybindingsManager::new();
            assert!(
                matches!(
                    panel.handle_key("enter", &kb),
                    crate::captain_view::CaptainAction::Compose { .. }
                ),
                "the picked model reaches the assign flow's access step"
            );
            drop(session);
            server.abort();
        })
        .await
        .expect("captain picker flow completes within ten seconds");
    }

    fn test_view() -> AgentView {
        AgentView::new(crate::theme::Theme::builtin(
            "prime",
            crate::theme::ColorMode::TrueColor,
        ))
    }

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn attached_session_cwd_prefers_an_existing_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        let snapshot = json!({"state": {"cwd": path}, "summary": {"cwd": "/nope/missing"}});
        assert_eq!(
            attached_session_cwd(&snapshot),
            Some(dir.path().to_path_buf())
        );
        let summary_only = json!({"state": {"cwd": ""}, "summary": {"cwd": path}});
        assert_eq!(
            attached_session_cwd(&summary_only),
            Some(dir.path().to_path_buf())
        );
        let missing = json!({"state": {"cwd": "/nope/missing-worktree"}});
        assert_eq!(attached_session_cwd(&missing), None);
        assert_eq!(attached_session_cwd(&json!({})), None);
    }

    #[tokio::test]
    async fn attach_adopts_the_attached_session_cwd() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (session, server, _requests) =
                mock_session(&dir.path().join("cwd.sock"), mpsc::unbounded_channel().0).await;
            assert_eq!(session.cwd, PathBuf::from("/tmp"));
            drop(session);
            server.abort();
        })
        .await
        .expect("attach completes within ten seconds");
    }

    #[test]
    fn transcript_switch_restores_each_sessions_reader_position() {
        let mut view = test_view();
        for index in 0..80 {
            view.push_entry(ChatEntry::User {
                text: format!("message {index}"),
            });
        }
        view.render_frame(60, 20);
        assert_eq!(view.chrome.nav.switch_transcript("a", None), None);
        view.scroll_to_entry(10, 0);
        view.render_frame(60, 20);
        let paused = view.reader_position();
        assert!(paused.is_some(), "a paused reader has a position");

        // a → b: a's place is saved and b (never seen) follows.
        assert_eq!(view.chrome.nav.switch_transcript("b", paused), None);
        // b → a (b was following): a's place comes back, b's stays unset.
        assert_eq!(view.chrome.nav.switch_transcript("a", None), paused);
        assert!(!view.chrome.nav.session_scroll.contains_key("b"));
        // A same-session rebuild keeps its place untouched.
        assert_eq!(view.chrome.nav.switch_transcript("a", None), None);
        let (entry, row) = paused.unwrap();
        view.scroll_to_entry(entry, row);
        assert!(!view.is_following());
    }

    #[tokio::test]
    async fn a_dock_opened_factory_page_closes_to_the_dock_and_a_direct_one_to_the_editor() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, _requests) = mock_session(
                &dir.path().join("dock-origin.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = test_view();

            session.activity_group = crate::chrome::ActivityGroup::Factory;
            session.subagents_focused = true;
            session.open_dock_group_view(&mut view);
            assert!(matches!(view.overlay, Some(Overlay::Factory(_))));
            assert!(view.chrome.nav.opened_from_dock);
            session
                .handle_factory_view_key(key(crossterm::event::KeyCode::Esc), &mut view)
                .await
                .unwrap();
            assert!(view.overlay.is_none());
            assert!(session.subagents_focused, "the dock takes the focus back");
            assert_eq!(
                session.activity_group,
                crate::chrome::ActivityGroup::Factory
            );

            session.subagents_focused = false;
            session.open_fsm_page(&mut view);
            assert!(matches!(view.overlay, Some(Overlay::Factory(_))));
            assert!(!view.chrome.nav.opened_from_dock);
            session
                .handle_factory_view_key(key(crossterm::event::KeyCode::Esc), &mut view)
                .await
                .unwrap();
            assert!(view.overlay.is_none());
            assert!(
                !session.subagents_focused,
                "a direct open returns to the editor"
            );
            drop(session);
            server.abort();
        })
        .await
        .expect("factory page flow completes within ten seconds");
    }

    #[tokio::test]
    async fn a_captain_opened_factory_page_returns_to_the_parked_panel() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, _requests) = mock_session(
                &dir.path().join("captain-origin.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = test_view();
            view.overlay = Some(Overlay::Captain(crate::captain_view::CaptainView::new(20)));
            session.open_factory_page_from_captain(
                &mut view,
                Some(crate::factory_view::CaptainRunOrigin {
                    project: "p1".into(),
                    member: Some("gate-check".into()),
                }),
            );
            let Some(Overlay::Factory(page)) = view.overlay.as_ref() else {
                panic!("the factory page mounts over the parked panel");
            };
            assert!(page.returns_to_captain());
            assert_eq!(page.captain_run().unwrap().project, "p1");
            assert!(session.captain_parked.is_some());
            session
                .handle_factory_view_key(key(crossterm::event::KeyCode::Left), &mut view)
                .await
                .unwrap();
            assert!(
                matches!(view.overlay, Some(Overlay::Captain(_))),
                "← returns to F6"
            );
            assert!(session.captain_parked.is_none());
            drop(session);
            server.abort();
        })
        .await
        .expect("captain origin flow completes within ten seconds");
    }

    #[tokio::test]
    async fn dock_enter_on_a_captain_sub_row_opens_the_captain_panel() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let (mut session, server, _requests) = mock_session(
                &dir.path().join("captain-item.sock"),
                mpsc::unbounded_channel().0,
            )
            .await;
            let mut view = test_view();
            session.captain_dock = Some(crate::chrome::CaptainDockState {
                project_id: "p1".into(),
                name: "alpha".into(),
                state: "active".into(),
                active: vec![
                    crate::chrome::CaptainDockActiveItem {
                        label: "gate-check".into(),
                        kind: "factory".into(),
                        state: "paused".into(),
                        state_text: "paused".into(),
                        step: None,
                    },
                    crate::chrome::CaptainDockActiveItem {
                        label: "ship".into(),
                        kind: "project_factory".into(),
                        state: "draft".into(),
                        state_text: "needs review".into(),
                        step: None,
                    },
                ],
                ..Default::default()
            });
            session.activity_group = crate::chrome::ActivityGroup::Captain;
            session.subagents_focused = true;
            session.update_subagent_summary(&mut view);
            let mut running = true;
            session
                .handle_key(
                    key(crossterm::event::KeyCode::Down),
                    &mut view,
                    &mut running,
                )
                .await
                .unwrap();
            assert_eq!(view.chrome.nav.dock_captain_item, Some(0));
            session
                .handle_key(
                    key(crossterm::event::KeyCode::Down),
                    &mut view,
                    &mut running,
                )
                .await
                .unwrap();
            assert_eq!(view.chrome.nav.dock_captain_item, Some(1));
            assert_eq!(session.dock_captain_item(1).unwrap().1.label, "ship");
            assert_eq!(
                view.chrome
                    .activity
                    .as_ref()
                    .map(crate::chrome::ActivityDock::selection),
                Some(crate::chrome::DockSelection::CaptainItem(1))
            );
            session
                .handle_key(
                    key(crossterm::event::KeyCode::Enter),
                    &mut view,
                    &mut running,
                )
                .await
                .unwrap();
            assert!(matches!(view.overlay, Some(Overlay::Captain(_))));
            assert!(view.chrome.nav.opened_from_dock);
            drop(session);
            server.abort();
        })
        .await
        .expect("captain sub-row flow completes within ten seconds");
    }
}
