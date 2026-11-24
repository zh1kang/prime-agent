//! The `/usage` panel's session side: the cached codexbar report, its background load, and
//! the panel's keys. A report younger than the cache TTL reopens without a new load.

use super::{key_event_to_id, AgentView, KeyEvent, Overlay, Result, SessionUi};
use crate::usage_view::{UsageEvent, UsageView, UsageViewAction};
use std::time::{Duration, Instant};

const USAGE_CACHE_TTL: Duration = Duration::from_secs(60);

/// One step of a background load, delivered to the run loop.
#[derive(Debug)]
pub(crate) struct UsageUpdate {
    /// The load that sent it; a step from a replaced load is dropped.
    generation: u64,
    event: UsageEvent,
    at: Instant,
}

/// The running load. It ends when its last provider cost settles.
#[derive(Debug)]
pub(super) struct UsageFetch {
    generation: u64,
    /// The usage list arrived, so the cache holds this load's report.
    pub(super) listed: bool,
    task: tokio::task::AbortHandle,
}

impl SessionUi {
    /// Open the panel from the cache; start a load when the cache is stale or `refresh` asks.
    pub(super) fn open_usage_view(&mut self, view: &mut AgentView, refresh: bool) {
        let stale = self
            .usage_cache
            .as_ref()
            .is_none_or(|(_, fetched_at)| fetched_at.elapsed() >= USAGE_CACHE_TTL);
        // A forced refresh replaces a running load; a stale cache joins it.
        if refresh || (stale && self.usage_fetch.is_none()) {
            self.restart_usage_fetch();
        }
        let (report, fetched_at) = self.usage_cache.clone().unzip();
        let refreshing = self.usage_fetch.as_ref().is_some_and(|fetch| !fetch.listed);
        view.overlay = Some(Overlay::Usage(UsageView::new(
            report,
            fetched_at,
            refreshing,
            view.chrome.cost_usd,
        )));
        self.dirty = true;
    }

    /// Start a load, replacing (and aborting) one in flight.
    pub(super) fn restart_usage_fetch(&mut self) {
        if let Some(fetch) = self.usage_fetch.take() {
            fetch.task.abort();
        }
        self.usage_generation += 1;
        let generation = self.usage_generation;
        let tx = self.usage_updates.clone();
        let task = tokio::spawn(async move {
            crate::usage_view::load_usage(|event| {
                // A closed receiver means the run loop is gone; nothing is left to update.
                let _ = tx.send(UsageUpdate {
                    generation,
                    event,
                    at: Instant::now(),
                });
            })
            .await;
        });
        self.usage_fetch = Some(UsageFetch {
            generation,
            listed: false,
            task: task.abort_handle(),
        });
    }

    /// Fold one load step into the cache and the open panel.
    pub(crate) fn apply_usage_update(&mut self, update: UsageUpdate, view: &mut AgentView) {
        let Some(fetch) = self
            .usage_fetch
            .as_mut()
            .filter(|fetch| fetch.generation == update.generation)
        else {
            return;
        };
        let panel = match view.overlay.as_mut() {
            Some(Overlay::Usage(panel)) => Some(panel),
            _ => None,
        };
        match update.event {
            UsageEvent::Listed(report) => {
                fetch.listed = true;
                self.usage_cache = Some((report.clone(), update.at));
                if let Some(panel) = panel {
                    panel.set_report(report, update.at);
                } else if let Some(Overlay::Router(router_panel)) = view.overlay.as_mut() {
                    router_panel.reload_telemetry(Some(&report));
                }
            }
            UsageEvent::Cost { index, cost } => {
                if let Some(panel) = panel {
                    panel.set_cost(index, cost.clone());
                }
                if let Some((report, _)) = self.usage_cache.as_mut() {
                    report.set_cost(index, cost);
                }
            }
        }
        let complete = fetch.listed
            && self
                .usage_cache
                .as_ref()
                .is_none_or(|(report, _)| report.pending_costs() == 0);
        if complete {
            self.usage_fetch = None;
        }
        self.dirty = true;
    }

    pub(super) fn handle_usage_view_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        let Some(Overlay::Usage(panel)) = view.overlay.as_mut() else {
            return Ok(());
        };
        match panel.handle_key(&id, view.editor.keybindings()) {
            UsageViewAction::None => {}
            UsageViewAction::Close => view.overlay = None,
            UsageViewAction::Refresh => {
                panel.set_refreshing();
                self.restart_usage_fetch();
            }
        }
        self.dirty = true;
        Ok(())
    }
}
