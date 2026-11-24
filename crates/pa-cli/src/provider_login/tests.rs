//! The provider-login unit battery (moved beside its module at the
//! 2026-09-30 file-size cap: the flows and their tests stay one tree).

use super::*;

#[test]
fn the_status_indicator_maps_the_ts_labels() {
    let stored_api_key = AuthCredential::ApiKey {
        key: "k".to_string(),
        prime_team: None,
    };
    // A matching stored credential is configured.
    assert_eq!(
        status_indicator(
            Some(&stored_api_key),
            &AuthStatus {
                source: Some(AuthSource::Stored),
                ..Default::default()
            },
            AuthType::ApiKey
        ),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Success,
            label: "configured".to_string()
        })
    );
    // A subscription credential on an api-key row warns.
    assert_eq!(
        status_indicator(
            Some(&AuthCredential::Oauth {
                access: "a".to_string(),
                refresh: None,
                expires: 1,
                account_id: None,
                endpoint: None,
                token_endpoint: None,
                client_id: None,
                resource: None,
                issuer: None,
                audience_mode: None,
                enterprise_url: None,
            }),
            &AuthStatus::default(),
            AuthType::ApiKey
        ),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Warning,
            label: "subscription configured".to_string()
        })
    );
    // An env key marks the api-key row configured.
    assert_eq!(
        status_indicator(
            None,
            &AuthStatus {
                source: Some(AuthSource::Environment),
                label: Some("OPENAI_API_KEY".to_string()),
                ..Default::default()
            },
            AuthType::ApiKey
        ),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Success,
            label: "env: OPENAI_API_KEY".to_string()
        })
    );
    // An unconfigured subscription row stays muted.
    assert_eq!(
        status_indicator(None, &AuthStatus::default(), AuthType::Oauth),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Muted,
            label: "unconfigured".to_string()
        })
    );
    // An unconfigured api-key row hides its meta (TS inline rule).
    assert_eq!(
        status_indicator(None, &AuthStatus::default(), AuthType::ApiKey),
        None
    );
    // A stale credential warns.
    assert_eq!(
        status_indicator(
            None,
            &AuthStatus {
                source: Some(AuthSource::Stale),
                label: Some("expired".to_string()),
                ..Default::default()
            },
            AuthType::Oauth
        ),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Warning,
            label: "expired".to_string()
        })
    );
}

#[test]
fn the_display_names_match_the_ts_map() {
    assert_eq!(display_name("openai"), "OpenAI");
    assert_eq!(display_name("google"), "Google Gemini");
    assert_eq!(display_name("prime-inference"), "Prime Inference");
    assert_eq!(display_name("a-custom-provider"), "a-custom-provider");
}

#[test]
fn the_api_key_login_rule_matches_ts() {
    let mut built_in = std::collections::HashSet::new();
    built_in.insert("anthropic".to_string());
    // The display-name map marks api-key logins.
    assert!(is_api_key_login_provider("openai", &built_in));
    // A provider the built-in set does not know is a custom api-key
    // provider.
    assert!(is_api_key_login_provider("my-endpoint", &built_in));
    // A built-in provider without a display name is not an api-key
    // login (its subscription flows apply).
    assert!(!is_api_key_login_provider("faux", &{
        let mut set = std::collections::HashSet::new();
        set.insert("faux".to_string());
        set
    }));
}

#[tokio::test]
async fn login_options_list_providers_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::env::remove_var("PRIME_AGENT_TRACES_API_KEY");
    std::env::remove_var("PRIME_API_KEY");
    let auth = ProviderAuth::new(dir.path(), agent.clone());
    let rows = auth.login_options().await;
    // The TS OAuth registry rows render; every subscription flow is
    // ported, so every row selects (the flows answer through the
    // panel — never an after-selection error wall).
    for (id, name) in SUBSCRIPTION_PROVIDERS {
        let row = rows
            .iter()
            .find(|row| row.id == id && row.name == name)
            .unwrap_or_else(|| panic!("the {id} subscription row renders"));
        assert!(
            row.available,
            "the {id} row's flow is ported and selectable"
        );
    }
    // The operator's 2026-09-24 directive: /login is providers only —
    // the service rows (MCP OAuth integrations, the web search
    // credential) never appear; the /mcp view owns MCP logins.
    assert!(!rows.iter().any(|row| row.id == "serper"));
    assert!(!rows.iter().any(|row| row.id.starts_with("mcp:")));
    // Prime Inference sorts first among the api-key rows (TS rule).
    assert!(
        rows.iter()
            .position(|row| row.id == PRIME_INFERENCE_PROVIDER_ID)
            < rows.iter().position(|row| row.id == "anthropic"),
        "prime-inference sorts before the other api-key rows"
    );
}

#[tokio::test]
async fn login_options_list_the_claude_account_and_api_key_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    let auth = ProviderAuth::new(dir.path(), agent.clone());
    let rows = auth.login_options().await;
    let account = rows
        .iter()
        .find(|row| row.id == "anthropic" && row.auth_type == AuthType::Oauth)
        .expect("the Claude account row renders");
    assert_eq!(account.name, "Anthropic (Claude Account)");
    assert_eq!(account.flow, AuthFlow::TerminalFlow);
    assert!(account.status.is_some());
    let api_key = rows
        .iter()
        .find(|row| row.id == "anthropic" && row.auth_type == AuthType::ApiKey)
        .expect("the Anthropic API-key row renders");
    assert_eq!(api_key.name, "Anthropic");
    assert_eq!(api_key.flow, AuthFlow::ApiKeyPrompt);

    let antigravity = rows
        .iter()
        .find(|row| row.id == "antigravity" && row.auth_type == AuthType::Oauth)
        .expect("the Antigravity subscription row renders");
    assert_eq!(antigravity.name, "Antigravity (Google Subscription)");
    assert_eq!(antigravity.flow, AuthFlow::TerminalFlow);
    assert!(antigravity.status.is_some());

    let cursor = rows
        .iter()
        .find(|row| row.id == "cursor" && row.auth_type == AuthType::Oauth)
        .expect("the Cursor account row renders");
    assert_eq!(cursor.name, "Cursor (Cursor Account)");
    assert_eq!(cursor.flow, AuthFlow::TerminalFlow);
    assert!(cursor.status.is_some());
}

#[test]
fn the_antigravity_and_cursor_statuses_follow_their_clis() {
    use pa_ai::antigravity::AntigravityCliAuthStatus;
    use pa_ai::cursor_cli::CursorCliAuthStatus;

    let (status, configured) = antigravity_account_status(false, None);
    assert_eq!(status.style, AuthStatusStyle::Muted);
    assert_eq!(status.label, "install agy CLI");
    assert!(!configured);

    let (status, configured) = antigravity_account_status(
        true,
        Some(&AntigravityCliAuthStatus {
            logged_in: true,
            model_count: 6,
            detail: None,
        }),
    );
    assert_eq!(status.style, AuthStatusStyle::Success);
    assert_eq!(status.label, "agy CLI (6 models)");
    assert!(configured);

    let (status, configured) = cursor_account_status(false, None);
    assert_eq!(status.style, AuthStatusStyle::Muted);
    assert_eq!(status.label, "install cursor-agent CLI");
    assert!(!configured);

    let (status, configured) = cursor_account_status(
        true,
        Some(&CursorCliAuthStatus {
            logged_in: true,
            account: Some("dev@example.com".to_string()),
            team: Some("Acme".to_string()),
        }),
    );
    assert_eq!(status.style, AuthStatusStyle::Success);
    assert_eq!(status.label, "dev@example.com (Acme)");
    assert!(configured);
}

#[test]
fn the_claude_account_status_follows_the_claude_cli() {
    use pa_ai::claude_cli::ClaudeCliAuthStatus;

    let (status, configured) = claude_account_status(false, None);
    assert_eq!(status.style, AuthStatusStyle::Muted);
    assert_eq!(status.label, "install claude CLI");
    assert!(!configured);

    let (status, configured) = claude_account_status(true, None);
    assert_eq!(status.style, AuthStatusStyle::Muted);
    assert_eq!(status.label, "unconfigured");
    assert!(!configured);

    let signed_out = ClaudeCliAuthStatus {
        logged_in: false,
        auth_method: None,
        subscription_type: None,
    };
    let (status, configured) = claude_account_status(true, Some(&signed_out));
    assert_eq!(status.label, "unconfigured");
    assert!(!configured);

    let team = ClaudeCliAuthStatus {
        logged_in: true,
        auth_method: Some("claude.ai".to_string()),
        subscription_type: Some("team".to_string()),
    };
    let (status, configured) = claude_account_status(true, Some(&team));
    assert_eq!(status.style, AuthStatusStyle::Success);
    assert_eq!(status.label, "claude.ai (team)");
    assert!(configured);

    let no_plan = ClaudeCliAuthStatus {
        subscription_type: None,
        ..team
    };
    assert_eq!(
        claude_account_status(true, Some(&no_plan)).0.label,
        "claude.ai"
    );
}

#[test]
fn the_claude_login_url_is_taken_from_cli_output() {
    assert_eq!(
        login_url("Open this URL: https://claude.ai/oauth/authorize?code=1 to continue"),
        Some("https://claude.ai/oauth/authorize?code=1")
    );
    assert_eq!(login_url("Opening browser to sign in"), None);
}

#[test]
fn a_finished_claude_login_removes_the_legacy_anthropic_oauth_token() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut storage = pa_core::auth::AuthStorage::create(dir.path());
    storage.set(
        "anthropic",
        AuthCredential::Oauth {
            access: "legacy".to_string(),
            refresh: None,
            expires: i64::MAX,
            endpoint: None,
            token_endpoint: None,
            client_id: None,
            resource: None,
            issuer: None,
            audience_mode: None,
            account_id: None,
            enterprise_url: None,
        },
    );
    storage.set(
        "openai",
        AuthCredential::ApiKey {
            key: "sk-openai".to_string(),
            prime_team: None,
        },
    );
    assert!(matches!(
        finish_claude_account_login(dir.path()),
        ProviderAuthOutcome::Status(message) if message == CLAUDE_SIGNED_IN
    ));
    let after = pa_core::auth::AuthStorage::create(dir.path()).get_all();
    assert!(after.credential("anthropic").is_none());
    assert!(after.credential("openai").is_some());

    // A saved Anthropic API key is the user's explicit choice and stays.
    storage.set(
        "anthropic",
        AuthCredential::ApiKey {
            key: "sk-ant-api03-saved".to_string(),
            prime_team: None,
        },
    );
    finish_claude_account_login(dir.path());
    assert!(pa_core::auth::AuthStorage::create(dir.path())
        .get_all()
        .credential("anthropic")
        .is_some());
}

/// The TS #2645 ban-risk warning no longer applies: an Anthropic
/// subscription (a stored OAuth login or an `sk-ant-oat` key) is served by
/// the official `claude` CLI, so pa never identifies as Claude Code itself
/// and no credential shape produces the warning.
#[tokio::test]
async fn the_anthropic_subscription_has_no_ban_risk_warning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    let auth = ProviderAuth::new(dir.path(), agent.clone());
    assert_eq!(auth.anthropic_subscription_warning().await, None);

    let mut storage = pa_core::auth::AuthStorage::create(&agent);
    for key in ["sk-ant-api03-plain", "sk-ant-oat-subscription"] {
        storage.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: key.to_string(),
                prime_team: None,
            },
        );
        assert_eq!(auth.anthropic_subscription_warning().await, None, "{key}");
    }
    storage.set(
        "anthropic",
        AuthCredential::Oauth {
            access: "a".to_string(),
            refresh: None,
            expires: i64::MAX,
            endpoint: None,
            token_endpoint: None,
            client_id: None,
            resource: None,
            issuer: None,
            audience_mode: None,
            account_id: None,
            enterprise_url: None,
        },
    );
    assert_eq!(auth.anthropic_subscription_warning().await, None);
}

#[tokio::test]
async fn login_stores_an_api_key_and_logout_removes_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    let auth = ProviderAuth::new(dir.path(), agent);
    let row = ProviderRow {
        id: "openai".to_string(),
        name: "OpenAI".to_string(),
        auth_type: AuthType::ApiKey,
        status: None,
        flow: AuthFlow::ApiKeyPrompt,
        configured: false,
        available: true,
    };
    match auth.login(&row, Some("sk-test")).await {
        ProviderAuthOutcome::Status(message) => {
            assert!(
                message.starts_with("Saved API key for OpenAI. Credentials saved to "),
                "the store status names the credential path: {message}"
            );
        }
        other => panic!("expected the store status, got {other:?}"),
    }
    // An empty key answers the TS error.
    assert_eq!(
        auth.login(&row, Some("")).await,
        ProviderAuthOutcome::Error(
            "Failed to save API key for OpenAI: API key cannot be empty.".to_string()
        )
    );
    // The logout lists the stored credential and removes it.
    let stored = auth.logout_options().await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, "openai");
    assert_eq!(
            auth.logout(&stored[0]).await,
            ProviderAuthOutcome::Status(
                "Removed stored API key for OpenAI. Environment variables and models.json config are unchanged."
                    .to_string()
            )
        );
    assert!(auth.logout_options().await.is_empty());
}

#[tokio::test]
async fn a_stored_web_search_key_pairs_with_the_logout_row() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    let auth = ProviderAuth::new(dir.path(), agent);
    // A key stored at the runtime's slot (the `/mcp` view's key flow
    // writes the same store through the same form).
    let row = ProviderRow {
        id: "serper".to_string(),
        name: "Serper (web search)".to_string(),
        auth_type: AuthType::ApiKey,
        status: None,
        flow: AuthFlow::ApiKeyPrompt,
        configured: false,
        available: true,
    };
    match auth.login(&row, Some("the-key")).await {
        ProviderAuthOutcome::Status(message) => {
            assert!(
                message.starts_with("Saved API key for Serper (web search)."),
                "the store status: {message}"
            );
        }
        other => panic!("expected the store status, got {other:?}"),
    }
    // The logout row pairs with the stored key: the `/logout` surface
    // lists it by its credential name and removes it.
    let stored = auth.logout_options().await;
    let row = stored
        .iter()
        .find(|row| row.id == "serper")
        .expect("the web-search credential's logout row");
    assert_eq!(row.name, "Serper (web search)");
    assert_eq!(
            auth.logout(row).await,
            ProviderAuthOutcome::Status(
                "Removed stored API key for Serper (web search). Environment variables and models.json config are unchanged."
                    .to_string()
            )
        );
    assert!(auth.logout_options().await.is_empty());
}
