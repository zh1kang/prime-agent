//! Headless e2e for the CLI-backed subscription rows in `/login` (Antigravity through `agy`,
//! Cursor through `cursor-agent`): selecting a row mounts the inline sign-in panel and runs the
//! row's flow on it, so the sign-in link, the progress, and the outcome render.
#![cfg(unix)]

mod support;

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use radiata_tui::interactive::{
    run_interactive, HeadlessPlan, HeadlessStep, InteractiveOptions, UiMode,
};
use radiata_tui::provider_auth::{
    AuthFlow, AuthType, ProviderAuthCommands, ProviderAuthCommandsHandle, ProviderAuthFuture,
    ProviderAuthOutcome, ProviderRow, ProviderRowsFuture, ProviderWarningFuture,
};

use support::{
    accept_within_idle_window, attach_response, create_response, options, serve_connection,
    write_json, MockSupervisor, Request,
};

impl MockSupervisor {
    fn serve(self) {
        let Some(stream) = accept_within_idle_window(&self.listener) else {
            return;
        };
        serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
            match kind {
                "create" => write_json(writer, &create_response(id)),
                "attach" => write_json(writer, &attach_response(id, "login session")),
                _ => return false,
            }
            true
        });
    }
}

fn row(id: &str, name: &str) -> ProviderRow {
    ProviderRow {
        id: id.to_string(),
        name: name.to_string(),
        auth_type: AuthType::Oauth,
        status: None,
        flow: AuthFlow::TerminalFlow,
        configured: false,
        available: true,
    }
}

/// The fake flows: the terminal path answers the silent cancel (as the product's does for an
/// OAuth row), and the panel path shows a link and progress before it settles.
struct CliAuth;

impl ProviderAuthCommands for CliAuth {
    fn login_options(&self) -> ProviderRowsFuture {
        Box::pin(async move {
            vec![
                row("antigravity", "Antigravity (Google Subscription)"),
                row("cursor", "Cursor (Cursor Account)"),
            ]
        })
    }

    fn logout_options(&self) -> ProviderRowsFuture {
        Box::pin(async move { Vec::new() })
    }

    fn login(&self, _provider: &ProviderRow, _api_key: Option<&str>) -> ProviderAuthFuture {
        Box::pin(async move { ProviderAuthOutcome::Cancelled })
    }

    fn login_on_panel(
        &self,
        provider: &ProviderRow,
        panel: radiata_tui::auth_panel::AuthPanelHandle,
    ) -> ProviderAuthFuture {
        let id = provider.id.clone();
        Box::pin(async move {
            panel.progress_line(format!("TEST checking {id} session"));
            panel.auth_url(&format!("https://example.test/{id}/signin"), None);
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            ProviderAuthOutcome::Status(format!("TEST signed in to {id}"))
        })
    }

    fn logout(&self, _provider: &ProviderRow) -> ProviderAuthFuture {
        Box::pin(async move { ProviderAuthOutcome::Cancelled })
    }

    fn anthropic_subscription_warning(&self) -> ProviderWarningFuture {
        Box::pin(async move { None })
    }
}

fn wait_render(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 10_000,
    }
}

fn run_login(search: &str, id: &str) -> Vec<String> {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let supervisor = MockSupervisor::bind(&socket);
    let handle = std::thread::spawn(move || supervisor.serve());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps: vec![
            HeadlessStep::Submit("/login".to_string()),
            wait_render("Cursor (Cursor Account)"),
            HeadlessStep::Type(search.to_string()),
            HeadlessStep::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            wait_render(&format!("TEST signed in to {id}")),
        ],
        width: 100,
        height: 30,
    };
    let outcome = runtime
        .block_on(run_interactive(
            InteractiveOptions {
                provider_auth: Some(ProviderAuthCommandsHandle(Arc::new(CliAuth))),
                ..options(socket)
            },
            UiMode::Headless(plan),
        ))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");
    outcome.frames
}

fn assert_panel_flow(frames: &[String], title: &str, id: &str) {
    let all = frames.join("\n");
    let panel = frames
        .iter()
        .find(|frame| frame.contains(title))
        .unwrap_or_else(|| panic!("the sign-in panel `{title}` mounted:\n{all}"));
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains(title)
                && frame.contains(&format!("example.test/{id}/signin"))),
        "the panel showed the sign-in link:\n{panel}"
    );
    assert!(
        all.contains(&format!("TEST checking {id} session")),
        "the panel showed the flow's progress:\n{all}"
    );
    assert!(
        all.contains(&format!("TEST signed in to {id}")),
        "the flow's outcome reached the transcript:\n{all}"
    );
}

#[test]
fn the_antigravity_row_runs_its_sign_in_on_the_panel() {
    let frames = run_login("Antigravity", "antigravity");
    assert_panel_flow(
        &frames,
        "Login to Antigravity (Google Subscription)",
        "antigravity",
    );
}

#[test]
fn the_cursor_row_runs_its_sign_in_on_the_panel() {
    let frames = run_login("Cursor", "cursor");
    assert_panel_flow(&frames, "Login to Cursor (Cursor Account)", "cursor");
}
