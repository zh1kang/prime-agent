//! The provider auth flows behind the TUI's `/login` and `/logout` (TS
//! `ProviderAuthFlows`): the provider catalog rows, the API-key store, the
//! MCP device flow, the Prime Inference login, and the subscription logins;
//! the Prime browser logins are not ported yet.

use std::path::{Path, PathBuf};

use pa_core::auth::{AuthCredential, AuthSource, AuthStatus};
use pa_core::models::ModelRegistry;
use pa_tui::provider_auth::{
    AuthFlow, AuthStatusIndicator, AuthStatusStyle, AuthType, ProviderAuthCommands,
    ProviderAuthFuture, ProviderAuthOutcome, ProviderRow, ProviderRowsFuture,
    ProviderWarningFuture,
};

/// The subscription login providers (Anthropic subscription uses the official `claude` CLI
/// directly instead of OAuth API spoofing).
const SUBSCRIPTION_PROVIDERS: [(&str, &str); 3] = [
    ("github-copilot", "GitHub Copilot"),
    ("openai-codex", "ChatGPT Plus/Pro (Codex Subscription)"),
    ("xai", "xAI (Grok)"),
];

/// Provider id -> display name; a known display name marks an API-key login provider.
const BUILT_IN_PROVIDER_DISPLAY_NAMES: &[(&str, &str)] = &[
    ("anthropic", "Anthropic"),
    ("amazon-bedrock", "Amazon Bedrock"),
    ("azure-openai-responses", "Azure OpenAI Responses"),
    ("cerebras", "Cerebras"),
    ("cloudflare-ai-gateway", "Cloudflare AI Gateway"),
    ("cloudflare-workers-ai", "Cloudflare Workers AI"),
    ("deepseek", "DeepSeek"),
    ("fireworks", "Fireworks"),
    ("google", "Google Gemini"),
    ("google-vertex", "Google Vertex AI"),
    ("groq", "Groq"),
    ("huggingface", "Hugging Face"),
    ("kimi-coding", "Kimi For Coding"),
    ("mistral", "Mistral"),
    ("minimax", "MiniMax"),
    ("minimax-cn", "MiniMax (China)"),
    ("moonshotai", "Moonshot AI"),
    ("moonshotai-cn", "Moonshot AI (China)"),
    ("opencode", "OpenCode Zen"),
    ("opencode-go", "OpenCode Go"),
    ("openai", "OpenAI"),
    ("openrouter", "OpenRouter"),
    ("prime-agent-traces", "Prime Agent Traces"),
    ("prime-inference", "Prime Inference"),
    ("vercel-ai-gateway", "Vercel AI Gateway"),
    ("xai", "xAI (Grok)"),
    ("zai", "ZAI"),
    ("xiaomi", "Xiaomi MiMo"),
    ("xiaomi-token-plan-cn", "Xiaomi MiMo Token Plan (China)"),
    (
        "xiaomi-token-plan-ams",
        "Xiaomi MiMo Token Plan (Amsterdam)",
    ),
    (
        "xiaomi-token-plan-sgp",
        "Xiaomi MiMo Token Plan (Singapore)",
    ),
];

const PRIME_INFERENCE_PROVIDER_ID: &str = "prime-inference";

/// The Claude account row: signs in through the official `claude` CLI, which then
/// serves Anthropic models. The `Anthropic` API-key row stays separate.
const CLAUDE_ACCOUNT_ROW: (&str, &str) = (
    pa_ai::claude_cli::ANTHROPIC_PROVIDER,
    "Anthropic (Claude Account)",
);

/// The Antigravity subscription row: reuses the `agy` / `antigravity` CLI session.
const ANTIGRAVITY_ACCOUNT_ROW: (&str, &str) = (
    pa_ai::antigravity::ANTIGRAVITY_PROVIDER,
    "Antigravity (Google Subscription)",
);

/// The Cursor subscription row: signs in through the `cursor-agent` CLI.
const CURSOR_ACCOUNT_ROW: (&str, &str) = (
    pa_ai::cursor_cli::CURSOR_PROVIDER,
    "Cursor (Cursor Account)",
);

/// The display name of a provider id (else the id itself).
fn display_name(provider_id: &str) -> String {
    BUILT_IN_PROVIDER_DISPLAY_NAMES
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map_or_else(|| provider_id.to_string(), |(_, name)| name.to_string())
}

/// The display-name map, or a provider the built-in model provider set does not know (custom
/// models.json entries).
fn is_api_key_login_provider(
    provider_id: &str,
    built_in_provider_ids: &std::collections::HashSet<String>,
) -> bool {
    if provider_id == pa_ai::antigravity::ANTIGRAVITY_PROVIDER
        || provider_id == pa_ai::cursor_cli::CURSOR_PROVIDER
    {
        return false;
    }
    if BUILT_IN_PROVIDER_DISPLAY_NAMES
        .iter()
        .any(|(id, _)| *id == provider_id)
    {
        return true;
    }
    !built_in_provider_ids.contains(provider_id)
}

/// The provider's status indicator: the stored credential's match, the env key, or the unconfigured
/// hidden meta.
fn status_indicator(
    credential: Option<&AuthCredential>,
    status: &AuthStatus,
    auth_type: AuthType,
) -> Option<AuthStatusIndicator> {
    // A stale credential shows its warning label.
    if status.source == Some(AuthSource::Stale) {
        return Some(AuthStatusIndicator {
            style: AuthStatusStyle::Warning,
            label: status
                .label
                .clone()
                .unwrap_or_else(|| "expired".to_string()),
        });
    }
    // A non-stored source: env keys and config mark api-key providers configured; subscription rows
    // stay "unconfigured".
    if let Some(source) = status.source {
        if source != AuthSource::Stored {
            return match auth_type {
                AuthType::ApiKey => Some(AuthStatusIndicator {
                    style: AuthStatusStyle::Success,
                    label: api_key_source_label(source, status.label.as_deref()),
                }),
                AuthType::Oauth => Some(AuthStatusIndicator {
                    style: AuthStatusStyle::Muted,
                    label: "unconfigured".to_string(),
                }),
            };
        }
    }
    if let Some(credential) = credential {
        let credential_type = match credential {
            AuthCredential::ApiKey { .. } | AuthCredential::McpStaticToken { .. } => {
                AuthType::ApiKey
            }
            AuthCredential::Oauth { .. } => AuthType::Oauth,
        };
        return Some(if credential_type == auth_type {
            AuthStatusIndicator {
                style: AuthStatusStyle::Success,
                label: "configured".to_string(),
            }
        } else {
            AuthStatusIndicator {
                style: AuthStatusStyle::Warning,
                label: match credential_type {
                    AuthType::Oauth => "subscription configured".to_string(),
                    AuthType::ApiKey => "API key configured".to_string(),
                },
            }
        });
    }
    if auth_type != AuthType::ApiKey {
        return Some(AuthStatusIndicator {
            style: AuthStatusStyle::Muted,
            label: "unconfigured".to_string(),
        });
    }
    // An unconfigured api-key row hides its meta (TS inline rule).
    None
}

/// The Claude account row's status from the `claude` CLI: install hint, signed-in
/// account label, or unconfigured. The second value is the row's `configured` flag.
fn claude_account_status(
    cli_installed: bool,
    status: Option<&pa_ai::claude_cli::ClaudeCliAuthStatus>,
) -> (AuthStatusIndicator, bool) {
    if !cli_installed {
        return (
            AuthStatusIndicator {
                style: AuthStatusStyle::Muted,
                label: "install claude CLI".to_string(),
            },
            false,
        );
    }
    match status {
        Some(status) if status.logged_in => {
            let method = status.auth_method.as_deref().unwrap_or("signed in");
            let label = match status.subscription_type.as_deref() {
                Some(plan) => format!("{method} ({plan})"),
                None => method.to_string(),
            };
            (
                AuthStatusIndicator {
                    style: AuthStatusStyle::Success,
                    label,
                },
                true,
            )
        }
        _ => (
            AuthStatusIndicator {
                style: AuthStatusStyle::Muted,
                label: "unconfigured".to_string(),
            },
            false,
        ),
    }
}

fn claude_account_row() -> ProviderRow {
    let (id, name) = CLAUDE_ACCOUNT_ROW;
    let installed = pa_ai::claude_cli::is_available();
    let auth_status = installed.then(pa_ai::claude_cli::auth_status).flatten();
    let (status, configured) = claude_account_status(installed, auth_status.as_ref());
    ProviderRow {
        id: id.to_string(),
        name: name.to_string(),
        auth_type: AuthType::Oauth,
        status: Some(status),
        flow: AuthFlow::TerminalFlow,
        configured,
        available: true,
    }
}

/// The Antigravity row's status from the `agy` / `antigravity` CLI.
fn antigravity_account_status(
    cli_installed: bool,
    status: Option<&pa_ai::antigravity::AntigravityCliAuthStatus>,
) -> (AuthStatusIndicator, bool) {
    if !cli_installed {
        return (
            AuthStatusIndicator {
                style: AuthStatusStyle::Muted,
                label: "install agy CLI".to_string(),
            },
            false,
        );
    }
    match status {
        Some(status) if status.logged_in => {
            let label = if status.model_count > 0 {
                format!("agy CLI ({} models)", status.model_count)
            } else {
                "agy CLI".to_string()
            };
            (
                AuthStatusIndicator {
                    style: AuthStatusStyle::Success,
                    label,
                },
                true,
            )
        }
        _ => (
            AuthStatusIndicator {
                style: AuthStatusStyle::Muted,
                label: "unconfigured".to_string(),
            },
            false,
        ),
    }
}

fn antigravity_account_row() -> ProviderRow {
    let (id, name) = ANTIGRAVITY_ACCOUNT_ROW;
    let installed = pa_ai::antigravity::is_available();
    let auth_status = installed
        .then(|| pa_ai::antigravity::auth_status(false))
        .flatten();
    let (status, configured) = antigravity_account_status(installed, auth_status.as_ref());
    ProviderRow {
        id: id.to_string(),
        name: name.to_string(),
        auth_type: AuthType::Oauth,
        status: Some(status),
        flow: AuthFlow::TerminalFlow,
        configured,
        available: true,
    }
}

/// The Cursor row's status from `cursor-agent status`.
fn cursor_account_status(
    cli_installed: bool,
    status: Option<&pa_ai::cursor_cli::CursorCliAuthStatus>,
) -> (AuthStatusIndicator, bool) {
    if !cli_installed {
        return (
            AuthStatusIndicator {
                style: AuthStatusStyle::Muted,
                label: "install cursor-agent CLI".to_string(),
            },
            false,
        );
    }
    match status {
        Some(status) if status.logged_in => {
            let account = status.account.as_deref().unwrap_or("signed in");
            let label = match status.team.as_deref() {
                Some(team) => format!("{account} ({team})"),
                None => account.to_string(),
            };
            (
                AuthStatusIndicator {
                    style: AuthStatusStyle::Success,
                    label,
                },
                true,
            )
        }
        _ => (
            AuthStatusIndicator {
                style: AuthStatusStyle::Muted,
                label: "unconfigured".to_string(),
            },
            false,
        ),
    }
}

fn cursor_account_row() -> ProviderRow {
    let (id, name) = CURSOR_ACCOUNT_ROW;
    let installed = pa_ai::cursor_cli::is_available();
    let auth_status = installed.then(pa_ai::cursor_cli::auth_status).flatten();
    let (status, configured) = cursor_account_status(installed, auth_status.as_ref());
    ProviderRow {
        id: id.to_string(),
        name: name.to_string(),
        auth_type: AuthType::Oauth,
        status: Some(status),
        flow: AuthFlow::TerminalFlow,
        configured,
        available: true,
    }
}

fn api_key_source_label(source: AuthSource, label: Option<&str>) -> String {
    match source {
        AuthSource::Environment => {
            format!("env: {}", label.unwrap_or("API key"))
        }
        AuthSource::PrimeCli => label.unwrap_or("Prime CLI").to_string(),
        AuthSource::Runtime => "runtime API key".to_string(),
        AuthSource::Fallback => "custom API key".to_string(),
        AuthSource::ModelsJsonKey => "key in models.json".to_string(),
        AuthSource::ModelsJsonCommand => "command in models.json".to_string(),
        AuthSource::Stored | AuthSource::Stale => "unconfigured".to_string(),
    }
}

/// Oauth before api key, then by name.
fn ts_row_order(a: &ProviderRow, b: &ProviderRow) -> std::cmp::Ordering {
    if a.auth_type != b.auth_type {
        return if a.auth_type == AuthType::Oauth {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        };
    }
    a.name.cmp(&b.name)
}

/// The provider auth surface against one daemon's shared directories.
#[derive(Clone)]
pub struct ProviderAuth {
    cwd: PathBuf,
    agent_dir: PathBuf,
}

impl ProviderAuth {
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        ProviderAuth {
            cwd: cwd.into(),
            agent_dir: agent_dir.into(),
        }
    }

    fn auth_storage(&self) -> pa_core::auth::AuthStorage {
        pa_core::auth::AuthStorage::create(&self.agent_dir)
    }

    /// The stored credential + auth status of one provider id.
    fn credential_status(&self, provider_id: &str) -> (Option<AuthCredential>, AuthStatus) {
        let auth = self.auth_storage();
        let credential = auth.get_all().credential(provider_id);
        let status = auth.get_auth_status(provider_id);
        (credential, status)
    }

    /// Anthropic subscription requests now route through the official `claude` CLI directly
    /// instead of spoofing Claude Code headers, so no ban-risk warning applies.
    fn anthropic_subscription_warning_blocking(&self) -> Option<&'static str> {
        None
    }
}

impl ProviderAuthCommands for ProviderAuth {
    /// TS `getLoginProviderOptions`: the subscription rows and the
    /// API-key model providers, sorted TS-style with prime-inference
    /// first. The service rows (the MCP OAuth integrations, the
    /// web-search credential) live on the `/mcp` view (the operator's
    /// 2026-09-24 `/login`-is-providers-only directive).
    fn login_options(&self) -> ProviderRowsFuture {
        let provider = self.clone();
        Box::pin(async move {
            // The row build locks the auth store and the MCP manager (blocking mutexes); keep them
            // off the async workers.
            tokio::task::spawn_blocking(move || provider.login_rows_blocking())
                .await
                .expect("the row build task ran")
        })
    }

    /// TS `getLogoutProviderOptions`: one row per stored credential, sorted by name.
    fn logout_options(&self) -> ProviderRowsFuture {
        let provider = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || provider.logout_rows_blocking())
                .await
                .expect("the row build task ran")
        })
    }

    /// The API-key store for prompted keys; the panel-driven flows route through
    /// [`ProviderAuth::login_on_panel`].
    fn login(&self, provider: &ProviderRow, api_key: Option<&str>) -> ProviderAuthFuture {
        let provider_row = provider.clone();
        let agent_dir = self.agent_dir.clone();
        let api_key = api_key.map(str::to_string);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || login_blocking(&provider_row, &agent_dir, api_key))
                .await
                .expect("the login task ran")
        })
    }

    /// The panel-driven flows (the MCP OAuth login, the Prime Inference login):
    /// the flow drives the inline auth panel and never touches the terminal.
    fn login_on_panel(
        &self,
        provider: &ProviderRow,
        panel: pa_tui::auth_panel::AuthPanelHandle,
    ) -> ProviderAuthFuture {
        let provider_row = provider.clone();
        let cwd = self.cwd.clone();
        let agent_dir = self.agent_dir.clone();
        Box::pin(async move {
            // The panel round-trips stay off the async workers (a prompt's answer arrives from the
            // TUI loop's thread).
            tokio::task::spawn_blocking(move || {
                login_blocking_on_panel(&provider_row, cwd, agent_dir, panel)
            })
            .await
            .expect("the login task ran")
        })
    }

    /// The ban-risk warning (#2645) for an active Anthropic subscription auth, for the session
    /// surface to show once per run.
    fn anthropic_subscription_warning(&self) -> ProviderWarningFuture {
        let provider = self.clone();
        Box::pin(async move {
            // The lookup is warning-only, so a hung `!command` credential must not
            // pin the session surface: TS caps command execution at 10s.
            let lookup = tokio::task::spawn_blocking(move || {
                provider.anthropic_subscription_warning_blocking()
            });
            match tokio::time::timeout(std::time::Duration::from_secs(10), lookup).await {
                Ok(joined) => joined.unwrap_or(None),
                Err(_) => None,
            }
        })
    }

    /// The message matches the credential's type; a missing credential is a no-op notice.
    fn logout(&self, provider: &ProviderRow) -> ProviderAuthFuture {
        let provider_row = provider.clone();
        let agent_dir = self.agent_dir.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || logout_blocking(&provider_row, &agent_dir))
                .await
                .expect("the logout task ran")
        })
    }
}

impl ProviderAuth {
    /// The login row build (blocking: the auth store and MCP manager
    /// locks must stay off the async workers). The operator's 2026-09-24
    /// directive keeps `/login` providers only: the service rows (the MCP
    /// OAuth integrations, the web-search credential) live on the `/mcp`
    /// view instead.
    fn login_rows_blocking(self) -> Vec<ProviderRow> {
        let provider = self;
        {
            let mut rows: Vec<ProviderRow> = Vec::new();

            // The subscription OAuth rows: every flow is ported, and the logins run through the
            // panel.
            for (id, name) in SUBSCRIPTION_PROVIDERS {
                let (credential, status) = provider.credential_status(id);
                rows.push(ProviderRow {
                    id: id.to_string(),
                    name: name.to_string(),
                    auth_type: AuthType::Oauth,
                    status: status_indicator(credential.as_ref(), &status, AuthType::Oauth),
                    flow: AuthFlow::TerminalFlow,
                    configured: status.configured,
                    available: true,
                });
            }

            // Probe CLI-backed subscription rows in parallel so `/login` stays snappy.
            let (claude_row, antigravity_row, cursor_row) = std::thread::scope(|scope| {
                let claude = scope.spawn(claude_account_row);
                let antigravity = scope.spawn(antigravity_account_row);
                let cursor = scope.spawn(cursor_account_row);
                (
                    claude.join().expect("claude account probe"),
                    antigravity.join().expect("antigravity account probe"),
                    cursor.join().expect("cursor account probe"),
                )
            });
            rows.push(claude_row);
            rows.push(antigravity_row);
            rows.push(cursor_row);

            // The API-key model providers.
            let registry = ModelRegistry::create(
                provider.auth_storage(),
                provider.agent_dir.join("models.json"),
            );
            let built_in_provider_ids: std::collections::HashSet<String> =
                pa_ai::models_generated::get_providers()
                    .into_iter()
                    .map(str::to_string)
                    .collect();
            let mut providers: Vec<String> = registry
                .get_all()
                .iter()
                .map(|model| model.provider.clone())
                .collect();
            providers.sort();
            providers.dedup();
            for provider_id in providers {
                if !is_api_key_login_provider(&provider_id, &built_in_provider_ids) {
                    continue;
                }
                let (credential, status) = provider.credential_status(&provider_id);
                // Prime Inference logs in through the Prime flow, not a pasted key.
                let flow = if provider_id == PRIME_INFERENCE_PROVIDER_ID {
                    AuthFlow::TerminalFlow
                } else {
                    AuthFlow::ApiKeyPrompt
                };
                rows.push(ProviderRow {
                    status: status_indicator(credential.as_ref(), &status, AuthType::ApiKey),
                    id: provider_id.clone(),
                    name: display_name(&provider_id),
                    auth_type: AuthType::ApiKey,
                    flow,
                    configured: status.configured,
                    available: true,
                });
            }

            // Configured first, prime-inference first among them, then oauth before api key by
            // name.
            rows.sort_by(|a, b| {
                let configured = |row: &ProviderRow| {
                    matches!(
                        row.status.as_ref().map(|status| status.style),
                        Some(AuthStatusStyle::Success)
                    )
                };
                configured(b).cmp(&configured(a)).then(ts_row_order(a, b))
            });
            let mut sorted: Vec<ProviderRow> = Vec::with_capacity(rows.len());
            let mut rest = rows;
            if let Some(index) = rest
                .iter()
                .position(|row| row.id == PRIME_INFERENCE_PROVIDER_ID)
            {
                sorted.push(rest.remove(index));
            }
            sorted.append(&mut rest);
            sorted
        }
    }

    /// The logout row build.
    fn logout_rows_blocking(self) -> Vec<ProviderRow> {
        let auth = self.auth_storage();
        let mut rows: Vec<ProviderRow> = Vec::new();
        for provider_id in auth.list() {
            let Some(credential) = auth.get_all().credential(&provider_id) else {
                continue;
            };
            let auth_type = match credential {
                AuthCredential::ApiKey { .. } | AuthCredential::McpStaticToken { .. } => {
                    AuthType::ApiKey
                }
                AuthCredential::Oauth { .. } => AuthType::Oauth,
            };
            let api_key_label = pa_core::mcp::API_KEY_CREDENTIALS
                .iter()
                .find(|(id, _)| *id == provider_id)
                .map(|(_, label)| *label);
            let name = if let Some(label) = api_key_label {
                label.to_string()
            } else if provider_id.starts_with("mcp:") {
                provider_id
                    .strip_prefix("mcp:")
                    .unwrap_or(&provider_id)
                    .to_string()
            } else {
                display_name(&provider_id)
            };
            rows.push(ProviderRow {
                id: provider_id,
                name,
                auth_type,
                status: Some(AuthStatusIndicator {
                    style: AuthStatusStyle::Success,
                    label: "configured".to_string(),
                }),
                flow: AuthFlow::TerminalFlow,
                // The stored-credential rows exist because the credential is there; credential
                // removal always runs.
                configured: true,
                available: true,
            });
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }
}

/// The login flow body for the panel-prompted key store; panel-driven rows route
/// to [`login_blocking_on_panel`], and an OAuth row reaching it cancels silently.
fn login_blocking(
    provider_row: &ProviderRow,
    agent_dir: &Path,
    api_key: Option<String>,
) -> ProviderAuthOutcome {
    if provider_row.auth_type == AuthType::Oauth {
        return ProviderAuthOutcome::Cancelled;
    }
    let Some(api_key) = api_key.filter(|key| !key.is_empty()) else {
        return ProviderAuthOutcome::Error(format!(
            "Failed to save API key for {}: API key cannot be empty.",
            provider_row.name
        ));
    };
    let mut auth = pa_core::auth::AuthStorage::create(agent_dir);
    auth.set(
        &provider_row.id,
        AuthCredential::ApiKey {
            key: api_key,
            prime_team: None,
        },
    );
    if let Some(error) = auth.drain_errors().pop() {
        return ProviderAuthOutcome::Error(format!("could not save the API key: {error}"));
    }
    ProviderAuthOutcome::Status(format!(
        "Saved API key for {}. Credentials saved to {}",
        provider_row.name,
        agent_dir.join("auth.json").display()
    ))
}

/// The login flow body for the panel-driven rows: the flow awaits its transport
/// AND the panel's prompt/picker replies.
fn login_blocking_on_panel(
    provider_row: &ProviderRow,
    cwd: PathBuf,
    agent_dir: PathBuf,
    panel: pa_tui::auth_panel::AuthPanelHandle,
) -> ProviderAuthOutcome {
    if let Some(server) = provider_row.id.strip_prefix("mcp:") {
        let auth = crate::mcp_login::TerminalMcpAuth::new(cwd, agent_dir);
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_or_else(
                |error: std::io::Error| format!("login failed: {error}"),
                |runtime| {
                    runtime.block_on(pa_tui::client_auth::run_mcp_auth_command(
                        &auth,
                        &format!("login {server}"),
                        panel,
                    ))
                },
            );
        return if outcome.starts_with("Usage:") {
            ProviderAuthOutcome::Error(outcome)
        } else {
            ProviderAuthOutcome::Status(outcome)
        };
    }
    // The Prime Inference login.
    if provider_row.id == PRIME_INFERENCE_PROVIDER_ID {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_or_else(
                |error| {
                    ProviderAuthOutcome::Error(format!(
                        "Failed to login to {}: {error}",
                        provider_row.name
                    ))
                },
                |runtime| {
                    runtime.block_on(crate::prime_inference_login::run_prime_inference_login(
                        crate::prime_inference_login::PrimeLoginInputs {
                            agent_dir: &agent_dir,
                            provider_name: &provider_row.name,
                            config: &pa_core::auth::resolve_prime_inference_auth_config(),
                            http: &pa_core::auth::ReqwestPrimeHttp,
                            prime_cli_config_path:
                                crate::prime_inference_login::prime_cli_config_path(&agent_dir)
                                    .as_deref(),
                            prime_team_id: std::env::var("PRIME_TEAM_ID").ok().as_deref(),
                            poll_interval_ms: None,
                        },
                        &crate::prime_inference_login::PanelPrimeLoginUi::new(panel),
                    ))
                },
            );
    }
    // The Codex Subscription login.
    if provider_row.id == pa_core::auth::OPENAI_CODEX_PROVIDER_ID {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_or_else(
                |error: std::io::Error| {
                    ProviderAuthOutcome::Error(format!(
                        "Failed to login to {}: {error}",
                        provider_row.name
                    ))
                },
                |runtime| {
                    runtime.block_on(
                        crate::codex_subscription_login::run_codex_subscription_login(
                            &agent_dir,
                            &provider_row.name,
                            &pa_ai::oauth::ReqwestCodexHttp::new(),
                            &crate::codex_subscription_login::PanelCodexLoginUi::new(panel),
                        ),
                    )
                },
            );
    }
    // The Claude account login runs the official `claude` CLI.
    if provider_row.id == pa_core::auth::ANTHROPIC_PROVIDER_ID
        && provider_row.auth_type == AuthType::Oauth
    {
        return claude_account_login(&agent_dir, &panel);
    }
    // The Antigravity subscription verifies/guides the `agy` CLI session.
    if provider_row.id == pa_ai::antigravity::ANTIGRAVITY_PROVIDER
        && provider_row.auth_type == AuthType::Oauth
    {
        return antigravity_account_login(&panel);
    }
    // The Cursor subscription runs `cursor-agent login`.
    if provider_row.id == pa_ai::cursor_cli::CURSOR_PROVIDER
        && provider_row.auth_type == AuthType::Oauth
    {
        return cursor_account_login(&panel);
    }
    // The GitHub Copilot login.
    if provider_row.id == pa_core::auth::GITHUB_COPILOT_PROVIDER_ID {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_or_else(
                |error: std::io::Error| {
                    ProviderAuthOutcome::Error(format!(
                        "Failed to login to {}: {error}",
                        provider_row.name
                    ))
                },
                |runtime| {
                    runtime.block_on(crate::subscription_login::run_github_copilot_login(
                        &agent_dir,
                        &provider_row.name,
                        &pa_ai::oauth::ReqwestProviderHttp::new(),
                        &crate::subscription_login::PanelSubscriptionLoginUi::new(
                            panel,
                            &provider_row.id,
                        ),
                    ))
                },
            );
    }
    // The xAI (Grok) login.
    if provider_row.id == pa_core::auth::XAI_PROVIDER_ID {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_or_else(
                |error: std::io::Error| {
                    ProviderAuthOutcome::Error(format!(
                        "Failed to login to {}: {error}",
                        provider_row.name
                    ))
                },
                |runtime| {
                    runtime.block_on(crate::subscription_login::run_xai_login(
                        &agent_dir,
                        &provider_row.name,
                        &pa_ai::oauth::ReqwestProviderHttp::new(),
                        &crate::subscription_login::PanelSubscriptionLoginUi::new(
                            panel,
                            &provider_row.id,
                        ),
                    ))
                },
            );
    }
    // Any other row answers the silent cancel.
    ProviderAuthOutcome::Cancelled
}

const CLAUDE_SIGNED_IN: &str =
    "Signed in to Claude Code (claude.ai). Anthropic models route through the official claude CLI.";

/// Signs in with `claude auth login`. The CLI owns the account session; pa stores
/// nothing and only removes a legacy Anthropic OAuth token so `/login` shows one
/// source of truth.
fn claude_account_login(
    agent_dir: &Path,
    panel: &pa_tui::auth_panel::AuthPanelHandle,
) -> ProviderAuthOutcome {
    let Some(bin) = pa_ai::claude_cli::claude_bin() else {
        return ProviderAuthOutcome::Error(
            "Claude Code CLI (claude) is not installed. Install it from https://claude.ai/code and try again."
                .into(),
        );
    };
    if pa_ai::claude_cli::auth_status().is_some_and(|status| status.logged_in) {
        return finish_claude_account_login(agent_dir);
    }

    let mut cmd = std::process::Command::new(&bin);
    pa_ai::claude_cli::remove_api_key_env(&mut cmd)
        .args(["auth", "login"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let exit = match run_streaming_cli_login(
        cmd,
        panel,
        "Running claude auth login. Finish the sign-in in your browser.",
        &format!("{} auth login", bin.display()),
    ) {
        Ok(Some(exit)) => exit,
        Ok(None) => return ProviderAuthOutcome::Cancelled,
        Err(message) => return ProviderAuthOutcome::Error(message),
    };

    if pa_ai::claude_cli::auth_status().is_some_and(|status| status.logged_in) {
        return finish_claude_account_login(agent_dir);
    }
    ProviderAuthOutcome::Error(if exit.success() {
        "claude auth login finished, but claude auth status does not report a signed-in account."
            .to_string()
    } else {
        format!("claude auth login failed ({exit}).")
    })
}

/// The `agy acp` auth method for a Google account sign-in.
const ANTIGRAVITY_LOGIN_METHOD: &str = "oauth-personal";
/// The bound on one Antigravity sign-in: the user finishes the browser step inside it.
const ANTIGRAVITY_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Signs in to Antigravity through the `agy acp` server's `authenticate` method.
///
/// A live `agy models` check runs first: an existing session is never replaced, and an
/// unknown session state never starts a sign-in.
fn antigravity_account_login(panel: &pa_tui::auth_panel::AuthPanelHandle) -> ProviderAuthOutcome {
    let Some(bin) = pa_ai::antigravity::antigravity_bin() else {
        return ProviderAuthOutcome::Error(
            "Antigravity CLI (agy) is not installed. Install Antigravity CLI and try again.".into(),
        );
    };
    antigravity_login_with(
        &bin,
        panel,
        || pa_ai::antigravity::auth_status(true),
        ANTIGRAVITY_LOGIN_TIMEOUT,
    )
}

/// [`antigravity_account_login`] with the CLI path, the live status check, and the bound
/// supplied by the caller.
fn antigravity_login_with(
    bin: &Path,
    panel: &pa_tui::auth_panel::AuthPanelHandle,
    status: impl Fn() -> Option<pa_ai::antigravity::AntigravityCliAuthStatus>,
    timeout: std::time::Duration,
) -> ProviderAuthOutcome {
    panel.progress_line("Checking Antigravity CLI (agy) account session...");
    match status() {
        None => {
            return ProviderAuthOutcome::Error(format!(
                "Could not check the Antigravity session: `{} models` did not answer. Sign-in did not start.",
                bin.display()
            ));
        }
        Some(status) if status.logged_in => {
            return ProviderAuthOutcome::Status(format!(
                "Already signed in to Antigravity (agy CLI, {} models available). To use a different account, sign out in agy first.",
                status.model_count
            ));
        }
        Some(_) => {}
    }
    if panel.cancelled() {
        return ProviderAuthOutcome::Cancelled;
    }

    let mut cmd = std::process::Command::new(bin);
    cmd.arg("acp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match run_antigravity_acp_login(cmd, panel, timeout) {
        Ok(true) => {}
        Ok(false) => return ProviderAuthOutcome::Cancelled,
        Err(message) => return ProviderAuthOutcome::Error(message),
    }
    if panel.cancelled() {
        return ProviderAuthOutcome::Cancelled;
    }
    match status() {
        Some(status) if status.logged_in => ProviderAuthOutcome::Status(format!(
            "Signed in to Antigravity (agy CLI, {} models available). Antigravity models route through the agy CLI.",
            status.model_count
        )),
        status => {
            let detail = status
                .and_then(|status| status.detail)
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default();
            ProviderAuthOutcome::Error(format!(
                "Antigravity sign-in finished, but `agy models` does not report a signed-in account{detail}."
            ))
        }
    }
}

/// One line from the `agy acp` child.
enum AcpLine {
    Stdout(String),
    Stderr(String),
}

/// Drive `agy acp` through `initialize` and `authenticate`, forwarding sign-in links and
/// progress into the auth panel. Returns `Ok(false)` if the user cancelled.
fn run_antigravity_acp_login(
    mut cmd: std::process::Command,
    panel: &pa_tui::auth_panel::AuthPanelHandle,
    timeout: std::time::Duration,
) -> Result<bool, String> {
    use std::io::Write as _;

    let mut child = cmd
        .spawn()
        .map_err(|error| format!("Failed to start agy acp: {error}"))?;
    let mut stdin = child.stdin.take().expect("agy acp stdin is piped");
    let (lines_tx, lines_rx) = std::sync::mpsc::channel::<AcpLine>();
    let stdout = child.stdout.take().expect("agy acp stdout is piped");
    let stderr = child.stderr.take().expect("agy acp stderr is piped");
    for (stream, is_stdout) in [
        (Box::new(stdout) as Box<dyn std::io::Read + Send>, true),
        (Box::new(stderr) as Box<dyn std::io::Read + Send>, false),
    ] {
        let lines_tx = lines_tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in std::io::BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let line = if is_stdout {
                    AcpLine::Stdout(line)
                } else {
                    AcpLine::Stderr(line)
                };
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(lines_tx);

    let stop = |child: &mut std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let send = |stdin: &mut std::process::ChildStdin, message: serde_json::Value| {
        writeln!(stdin, "{message}").and_then(|()| stdin.flush())
    };
    if let Err(error) = send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": 1, "clientCapabilities": {} },
        }),
    ) {
        stop(&mut child);
        return Err(format!("Failed to talk to agy acp: {error}"));
    }
    panel.progress_line("Starting the Antigravity sign-in...");

    let deadline = std::time::Instant::now() + timeout;
    let mut last_stderr: Option<String> = None;
    loop {
        if panel.cancelled() {
            stop(&mut child);
            return Ok(false);
        }
        if std::time::Instant::now() >= deadline {
            stop(&mut child);
            return Err(format!(
                "Antigravity sign-in timed out after {} minutes.",
                timeout.as_secs() / 60
            ));
        }
        let line = match lines_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(line) => line,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let exit = child
                    .wait()
                    .map(|exit| exit.to_string())
                    .unwrap_or_default();
                let detail = last_stderr
                    .map(|line| format!(": {line}"))
                    .unwrap_or_default();
                return Err(format!(
                    "agy acp exited before the sign-in finished ({exit}){detail}"
                ));
            }
        };
        let text = match &line {
            AcpLine::Stdout(text) | AcpLine::Stderr(text) => text.trim(),
        };
        if text.is_empty() {
            continue;
        }
        let message = match &line {
            AcpLine::Stdout(_) => serde_json::from_str::<serde_json::Value>(text).ok(),
            AcpLine::Stderr(_) => None,
        };
        let Some(message) = message else {
            // Plain output: the CLI's sign-in instructions and link.
            match login_url(text) {
                Some(url) => panel.auth_url(url, Some(text)),
                None => panel.progress_line(text),
            }
            if matches!(line, AcpLine::Stderr(_)) {
                last_stderr = Some(text.to_string());
            }
            continue;
        };
        let id = message.get("id").and_then(serde_json::Value::as_i64);
        if message.get("method").is_some() {
            // A request from the agent: this client serves none, so answer it to keep the
            // server from waiting. A notification may carry the sign-in link.
            if let Some(url) = login_url(text) {
                panel.auth_url(url, None);
            }
            if let Some(request_id) = message.get("id") {
                let _ = send(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": { "code": -32601, "message": "method not supported by this client" },
                    }),
                );
            }
            continue;
        }
        if let Some(error) = message.get("error") {
            stop(&mut child);
            let detail = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("Antigravity sign-in failed: {detail}"));
        }
        match id {
            Some(1) => {
                let methods = message
                    .pointer("/result/authMethods")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let supported = methods.iter().any(|method| {
                    method.get("id").and_then(serde_json::Value::as_str)
                        == Some(ANTIGRAVITY_LOGIN_METHOD)
                });
                if !supported {
                    stop(&mut child);
                    return Err(
                        "This agy version does not offer Google sign-in over ACP. Run `agy` in a terminal and sign in there, then select this row again."
                            .into(),
                    );
                }
                if let Err(error) = send(
                    &mut stdin,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "authenticate",
                        "params": { "methodId": ANTIGRAVITY_LOGIN_METHOD },
                    }),
                ) {
                    stop(&mut child);
                    return Err(format!("Failed to talk to agy acp: {error}"));
                }
                panel.progress_line("Waiting for the Google sign-in. Finish it in your browser.");
            }
            Some(2) => {
                stop(&mut child);
                return Ok(true);
            }
            _ => {}
        }
    }
}

/// Signs in with `cursor-agent login`.
fn cursor_account_login(panel: &pa_tui::auth_panel::AuthPanelHandle) -> ProviderAuthOutcome {
    let Some(bin) = pa_ai::cursor_cli::cursor_bin() else {
        return ProviderAuthOutcome::Error(
            "Cursor CLI (cursor-agent) is not installed. Install it from https://cursor.com/cli and try again."
                .into(),
        );
    };
    if let Some(status) = pa_ai::cursor_cli::auth_status().filter(|status| status.logged_in) {
        let who = status.account.as_deref().unwrap_or("Cursor");
        return ProviderAuthOutcome::Status(format!(
            "Signed in to Cursor ({who}). Cursor models route through the cursor-agent CLI."
        ));
    }

    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("login")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let exit = match run_streaming_cli_login(
        cmd,
        panel,
        "Running cursor-agent login. Finish the sign-in in your browser.",
        &format!("{} login", bin.display()),
    ) {
        Ok(Some(exit)) => exit,
        Ok(None) => return ProviderAuthOutcome::Cancelled,
        Err(message) => return ProviderAuthOutcome::Error(message),
    };

    if let Some(status) = pa_ai::cursor_cli::auth_status().filter(|status| status.logged_in) {
        let who = status.account.as_deref().unwrap_or("Cursor");
        return ProviderAuthOutcome::Status(format!(
            "Signed in to Cursor ({who}). Cursor models route through the cursor-agent CLI."
        ));
    }
    ProviderAuthOutcome::Error(if exit.success() {
        "cursor-agent login finished, but cursor-agent status does not report a signed-in account."
            .to_string()
    } else {
        format!("cursor-agent login failed ({exit}).")
    })
}

/// Run a browser-login CLI subprocess while forwarding lines/URLs into the auth panel.
/// Returns `Ok(None)` if cancelled by the user.
fn run_streaming_cli_login(
    mut cmd: std::process::Command,
    panel: &pa_tui::auth_panel::AuthPanelHandle,
    progress_banner: &str,
    label: &str,
) -> Result<Option<std::process::ExitStatus>, String> {
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("Failed to start {label}: {error}"))?;
    panel.progress_line(progress_banner);

    let (lines_tx, lines_rx) = std::sync::mpsc::channel::<String>();
    let _readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|out| Box::new(out) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|err| Box::new(err) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|stream| {
        let lines_tx = lines_tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in std::io::BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        })
    })
    .collect();
    drop(lines_tx);

    let forward = |line: String| {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match login_url(line) {
            Some(url) => panel.auth_url(url, Some(line)),
            None => panel.progress_line(line),
        }
    };
    let exit = loop {
        while let Ok(line) = lines_rx.try_recv() {
            forward(line);
        }
        if panel.cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        match child.try_wait() {
            Ok(Some(exit)) => break exit,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(error) => {
                let _ = child.kill();
                return Err(format!("Failed waiting for {label}: {error}"));
            }
        }
    };
    // Drain any buffered lines up to a short grace window; do not block forever on reader
    // threads in case a browser helper grandchild inherited stdout/stderr.
    let drain_until = std::time::Instant::now() + std::time::Duration::from_millis(300);
    while let Some(remaining) = drain_until.checked_duration_since(std::time::Instant::now()) {
        match lines_rx.recv_timeout(remaining) {
            Ok(line) => forward(line),
            Err(_) => break,
        }
    }
    Ok(Some(exit))
}

/// The first `http(s)://` URL in a CLI output line.
fn login_url(line: &str) -> Option<&str> {
    let start = line.find("https://").or_else(|| line.find("http://"))?;
    line[start..].split_whitespace().next()
}

fn finish_claude_account_login(agent_dir: &Path) -> ProviderAuthOutcome {
    let mut auth = pa_core::auth::AuthStorage::create(agent_dir);
    if matches!(
        auth.get_all()
            .credential(pa_core::auth::ANTHROPIC_PROVIDER_ID),
        Some(AuthCredential::Oauth { .. })
    ) {
        auth.remove(pa_core::auth::ANTHROPIC_PROVIDER_ID);
        if let Some(error) = auth.drain_errors().pop() {
            return ProviderAuthOutcome::Error(format!(
                "Signed in to Claude Code, but could not remove the old Anthropic OAuth token: {error}"
            ));
        }
    }
    ProviderAuthOutcome::Status(CLAUDE_SIGNED_IN.to_string())
}

fn logout_blocking(provider_row: &ProviderRow, agent_dir: &Path) -> ProviderAuthOutcome {
    let mut auth = pa_core::auth::AuthStorage::create(agent_dir);
    if auth.get_all().get(&provider_row.id).is_none() {
        return ProviderAuthOutcome::Status(format!("{} is not configured.", provider_row.name));
    }
    auth.logout(&provider_row.id);
    if let Some(error) = auth.drain_errors().pop() {
        return ProviderAuthOutcome::Error(format!("Logout failed: {error}"));
    }
    match provider_row.auth_type {
        AuthType::Oauth => ProviderAuthOutcome::Status(format!(
            "Logged out of {}",
            provider_row.name
        )),
        AuthType::ApiKey => ProviderAuthOutcome::Status(format!(
            "Removed stored API key for {}. Environment variables and models.json config are unchanged.",
            provider_row.name
        )),
    }
}

#[cfg(test)]
mod tests;
