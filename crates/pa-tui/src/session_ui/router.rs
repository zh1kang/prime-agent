//! The `/router` panel's session side: opens the Ramp V2 model router TUI,
//! keeps its pool telemetry synced with the `/usage` cache, and triggers a
//! fresh `codexbar` reload on `r`.

use super::{key_event_to_id, AgentView, KeyEvent, Overlay, Result, SessionUi};
use crate::router_view::{RouterView, RouterViewAction};

impl SessionUi {
    pub(super) fn open_router_view(&mut self, view: &mut AgentView, refresh: bool) {
        let stale = self
            .usage_cache
            .as_ref()
            .is_none_or(|(_, fetched_at)| fetched_at.elapsed() >= std::time::Duration::from_secs(60));
        if refresh || (stale && self.usage_fetch.is_none()) {
            self.restart_usage_fetch();
        }
        let report = self.usage_cache.as_ref().map(|(report, _)| report);
        let refreshing = self.usage_fetch.as_ref().is_some_and(|fetch| !fetch.listed);
        view.overlay = Some(Overlay::Router(RouterView::open(report, refreshing)));
        self.dirty = true;
    }

    pub(super) fn handle_router_view_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        let Some(Overlay::Router(panel)) = view.overlay.as_mut() else {
            return Ok(());
        };
        match panel.handle_key(&id, view.editor.keybindings()) {
            RouterViewAction::None => {}
            RouterViewAction::Saved => {}
            RouterViewAction::Refresh => {
                panel.set_refreshing();
                self.restart_usage_fetch();
            }
            RouterViewAction::Close => view.overlay = None,
        }
        self.dirty = true;
        Ok(())
    }
}
