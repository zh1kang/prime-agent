//! The daemon's agent-inbox seams: the settings gate (`agentInbox.*`), the
//! engine's per-session store slot the worker's notice sink and reminder
//! read, and the periodic unread-reminder task. The store itself, its
//! journal, and the `rlm.inbox.*` host requests live in radiata-core
//! (`session_engine::inbox`).

use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;

use radiata_core::session_engine::inbox::SessionInbox;

use crate::agent_engine::AgentSessionEngine;
use crate::journal::WorkerRecoveryJournal;
use crate::worker::SessionCore;

/// The poll period while the reminder is disabled (`reminderIntervalMs`
/// of `0`): a later settings flip still arms it.
const INBOX_REMINDER_DISABLED_POLL_MS: u64 = 60_000;

impl AgentSessionEngine {
    /// The current session's inbox store, set at every session build.
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn set_inbox(&self, inbox: Arc<SessionInbox>) {
        *self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(inbox);
    }

    /// Drop the current session's inbox store (the retire seam: a
    /// retired session's store never serves a replacement's rebuild).
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn clear_inbox(&self) {
        *self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Install the worker's post-drain seam for the inbox (the wake-row
    /// refresh): every session build copies it onto the freshly opened
    /// store, and the kernel's read handler runs it after each drain.
    ///
    /// # Panics
    ///
    /// Panics when the read-hook slot lock is poisoned.
    pub(crate) fn set_inbox_read_hook(&self, hook: radiata_core::session_engine::inbox::InboxReadHook) {
        *self
            .inbox_read_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    /// The installed post-drain seam, when the worker set one.
    ///
    /// # Panics
    ///
    /// Panics when the read-hook slot lock is poisoned.
    pub(crate) fn inbox_read_hook(&self) -> Option<radiata_core::session_engine::inbox::InboxReadHook> {
        self.inbox_read_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The current session's inbox store, when a session is built and its
    /// model can read it. A CLI-backed model runs no RLM kernel, so it can
    /// never call `rlm.inbox.read()`: it answers `None`, which every
    /// router treats like `agentInbox.enabled: false` (direct notice rows,
    /// no wake rows, no unread reminder).
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn inbox(&self) -> Option<Arc<SessionInbox>> {
        if self.is_cli_backed_model() {
            return None;
        }
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the session's live model streams through an external CLI
    /// (`antigravity-cli`, `claude-cli`, `cursor-cli`).
    pub(crate) fn is_cli_backed_model(&self) -> bool {
        self.session_model()
            .is_ok_and(|model| is_cli_backed_api(&model.api))
    }

    /// Whether the given store is still the session's published inbox: the
    /// reminder cycle (and any read-and-queue admission) verifies its
    /// captured store under the slot's own mutex, so a replacement that
    /// installed the next session's store never admits over the retired
    /// one's count.
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn is_live_inbox(&self, inbox: &Arc<SessionInbox>) -> bool {
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, inbox))
    }

    /// The identity-checked durable push: the slot identity is re-checked
    /// and the notice appended under the SLOT's own mutex (the same lock the
    /// retire's [`Self::clear_inbox`] takes), so a concurrent retire — or a
    /// `create` replacement that installs the next session's store — either
    /// waits for this append (the replacement's build can only open the
    /// journal after the clear, with the record already on disk) or lands
    /// first and this call answers `None` (the sender retries into the
    /// replacement store). A retired store never takes an append that could
    /// race the next build's reopen and compaction of the same journal, and
    /// a delivery can never succeed against a store the replacement cannot
    /// read.
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn push_to_live_inbox(
        &self,
        inbox: &Arc<SessionInbox>,
        notice: radiata_core::session_engine::inbox::InboxNotice,
        at_ms: u64,
    ) -> Option<radiata_core::session_engine::inbox::InboxMessage> {
        let slot = self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, inbox))
        {
            return None;
        }
        Some(inbox.push(notice, at_ms))
    }

    /// The identity-checked withdrawal of a heartbeat job's waiting
    /// fires, under the slot's mutex like [`Self::push_to_live_inbox`]:
    /// a retired store never journals a withdrawal that could race the
    /// replacement's reopen of the same journal. `None` when the store
    /// is no longer the session's published inbox.
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn withdraw_heartbeat_from_live_inbox(
        &self,
        inbox: &Arc<SessionInbox>,
        job_id: &str,
    ) -> Option<Vec<radiata_core::session_engine::inbox::InboxMessage>> {
        let slot = self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, inbox))
        {
            return None;
        }
        Some(inbox.withdraw_heartbeat(job_id))
    }
}

/// Whether an api id names a CLI-backed provider, which runs no RLM kernel
/// and so cannot read the agent inbox.
pub(crate) fn is_cli_backed_api(api: &str) -> bool {
    matches!(
        api,
        radiata_ai::antigravity::ANTIGRAVITY_API
            | radiata_ai::claude_cli::CLAUDE_CLI_API
            | radiata_ai::cursor_cli::CURSOR_CLI_API
    )
}

/// `agentInbox.enabled` (default on): the kill switch that restores
/// direct notice-row steering.
pub(crate) fn inbox_enabled(cwd: &Path, agent_dir: &Path) -> bool {
    radiata_core::settings::SettingsManager::create(cwd, agent_dir).get_agent_inbox_enabled()
}

/// `agentInbox.reminderIntervalMs` (default 10 minutes, `0` disables).
pub(crate) fn reminder_interval_ms(cwd: &Path, agent_dir: &Path) -> u64 {
    radiata_core::settings::SettingsManager::create(cwd, agent_dir)
        .get_agent_inbox_reminder_interval_ms()
}

/// The periodic unread reminder: while the session's inbox stays
/// non-empty, one steering row repeats the waiting count every
/// `reminderIntervalMs`; the interval re-reads each cycle — including
/// after the wait, so a flip to `0` never admits — and an empty inbox,
/// a `0` interval, a closed session, or `agentInbox.enabled: false`
/// skips the cycle (the period resets when it drains; a closed session
/// keeps polling so a later session on the worker re-arms the loop).
pub(crate) fn spawn_inbox_reminder(
    engine: Weak<AgentSessionEngine>,
    core: &Arc<Mutex<SessionCore>>,
    work_notify: &Arc<Notify>,
    recovery: &Arc<Mutex<Option<WorkerRecoveryJournal>>>,
    agent_dir: &std::path::Path,
) {
    let core = Arc::clone(core);
    let work_notify = Arc::clone(work_notify);
    let recovery = Arc::clone(recovery);
    let agent_dir = agent_dir.to_path_buf();
    tokio::spawn(async move {
        loop {
            let interval_ms = match engine.upgrade() {
                Some(engine) => reminder_interval_ms(&engine.cwd(), &agent_dir),
                None => return,
            };
            let wait_ms = if interval_ms == 0 {
                INBOX_REMINDER_DISABLED_POLL_MS
            } else {
                interval_ms
            };
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            // `0` disables the reminder: this cycle admits nothing; the
            // poll re-reads the interval next cycle, so a later settings
            // flip re-arms it.
            if interval_ms == 0 {
                continue;
            }
            let Some(engine) = engine.upgrade() else {
                return;
            };
            // The interval re-reads after the wait too: a flip to `0`
            // while this cycle slept never admits (the sample above only
            // chose the wait).
            if reminder_interval_ms(&engine.cwd(), &agent_dir) == 0 {
                continue;
            }
            // A closed session skips the cycle instead of ending the
            // loop: the worker outlives its sessions, so a later session
            // on the same worker must re-arm the reminder.
            if engine.session_is_closed() {
                continue;
            }
            let Some(inbox) = engine.inbox() else {
                continue;
            };
            // The kill switch re-reads each cycle: `agentInbox.enabled:
            // false` never wakes the agent (the notice sink applies the
            // same setting per notice).
            if !inbox_enabled(&engine.cwd(), &agent_dir) {
                continue;
            }
            let closed = {
                let engine = std::sync::Arc::clone(&engine);
                move || engine.session_is_closed()
            };
            crate::worker::admit_inbox_reminder(
                &recovery,
                &core,
                &work_notify,
                &inbox,
                closed,
                Some(engine.as_ref()),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_engine::AgentEngineConfig;

    fn engine_on(
        provider: Option<&str>,
        model: Option<&str>,
    ) -> (AgentSessionEngine, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir,
            provider: provider.map(str::to_owned),
            model: model.map(str::to_owned),
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: None,
            cron_store: None,
            queued_steering_probe: None,
        })
        .unwrap();
        (engine, dir)
    }

    #[test]
    fn cli_backed_apis_are_recognized() {
        assert!(is_cli_backed_api(radiata_ai::antigravity::ANTIGRAVITY_API));
        assert!(is_cli_backed_api(radiata_ai::claude_cli::CLAUDE_CLI_API));
        assert!(is_cli_backed_api(radiata_ai::cursor_cli::CURSOR_CLI_API));
        assert!(!is_cli_backed_api("anthropic-messages"));
        assert!(!is_cli_backed_api("openai-responses"));
    }

    /// A CLI-backed session never exposes its store: the notice sink, the
    /// agent-message and heartbeat routes, and the reminder cycle all see
    /// no inbox, so no `[inbox …]` wake row or `[inbox reminder]` loop
    /// ever asks it for an `rlm.inbox.read()` it cannot make.
    #[test]
    fn cli_backed_session_exposes_no_inbox_for_reminders() {
        let (engine, _dir) = engine_on(Some("antigravity"), Some("gemini-4-neon"));
        assert!(engine.is_cli_backed_model());
        let inbox = Arc::new(SessionInbox::in_memory());
        engine.set_inbox(Arc::clone(&inbox));
        assert!(engine.inbox().is_none());
        assert!(
            engine.is_live_inbox(&inbox),
            "the slot itself still holds the store"
        );
    }

    #[test]
    fn unresolved_model_keeps_the_inbox() {
        let (engine, _dir) = engine_on(Some("no-such-provider"), Some("no-such-model"));
        assert!(!engine.is_cli_backed_model());
        engine.set_inbox(Arc::new(SessionInbox::in_memory()));
        assert!(engine.inbox().is_some());
    }
}
