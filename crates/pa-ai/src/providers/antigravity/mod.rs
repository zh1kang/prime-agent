//! Antigravity CLI (`agy`) streaming provider for Prime Agent.
//!
//! Surfaces Antigravity models (Gemini 4 Neon/Argon/Hydrogen, Gemini 3.8 Flash,
//! Claude Opus/Sonnet 5.5 via Antigravity, etc.) directly in `ModelRegistry` and
//! streams completions through the local Antigravity CLI (`agy` or T3 Code's
//! managed `antigravity` binary).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use pa_types::JsNumber;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStream, AssistantMessageEventWriter,
};
use crate::registry::Provider;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneStopReason,
    ErrorStopReason, Message, Model, ModelCost, ModelInput, ModelThinkingLevel,
    SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingLevelMap,
    ToolResultMessage, Usage, UsageCost, UserMessageContent,
};
use crate::utils_inner::diagnostics::now_ms;

/// Wire API identifier for Antigravity CLI models in `ModelRegistry`.
pub const ANTIGRAVITY_API: &str = "antigravity-cli";
/// Provider identifier for Antigravity models in `ModelRegistry`.
pub const ANTIGRAVITY_PROVIDER: &str = "antigravity";

const MODEL_CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
struct CachedConversation {
    conversation_id: String,
    model_id: String,
    message_count: usize,
}

fn sessions() -> &'static Mutex<HashMap<String, CachedConversation>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, CachedConversation>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn model_cache() -> &'static Mutex<Option<(Instant, Vec<Model>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Vec<Model>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn t3_antigravity_bin() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    let platform_dir = home
        .join(".t3/tools/antigravity-acp")
        .join(format!("{os}-{arch}"));
    let binary_name = if cfg!(windows) {
        "antigravity.exe"
    } else {
        "antigravity"
    };
    if let Ok(raw) = std::fs::read_to_string(platform_dir.join("active.json")) {
        if let Ok(val) = serde_json::from_str::<Value>(&raw) {
            if let Some(release) = val["releaseId"].as_str().filter(|s| !s.is_empty()) {
                let candidate = platform_dir
                    .join("versions")
                    .join(release)
                    .join(binary_name);
                if is_executable_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    let versions = std::fs::read_dir(platform_dir.join("versions")).ok()?;
    let mut found: Vec<_> = versions
        .flatten()
        .map(|entry| entry.path().join(binary_name))
        .filter(|path| is_executable_file(path))
        .collect();
    found.sort();
    found.pop()
}

/// Fast filesystem lookup for the Antigravity CLI executable (`PA_ANTIGRAVITY_BIN`,
/// `agy`, `~/.local/bin/agy`, `antigravity`, or T3 Code's managed binary).
#[must_use]
pub fn antigravity_bin() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("PA_ANTIGRAVITY_BIN") {
        let trimmed = explicit.trim();
        if trimmed.eq_ignore_ascii_case("none") || trimmed == "0" {
            return None;
        }
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Some(found) = find_in_path("agy") {
        return Some(found);
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let local = home.join(".local/bin/agy");
        if is_executable_file(&local) {
            return Some(local);
        }
    }
    if let Some(found) = find_in_path("antigravity") {
        return Some(found);
    }
    t3_antigravity_bin()
}

/// Whether an Antigravity CLI executable is installed on this machine.
#[must_use]
pub fn is_available() -> bool {
    antigravity_bin().is_some()
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for ch in chars.by_ref() {
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn build_thinking_map(efforts: &[&str]) -> Option<ThinkingLevelMap> {
    if efforts.is_empty() {
        return None;
    }
    let mut map = BTreeMap::new();
    for (level, wire) in [
        (ModelThinkingLevel::Off, "off"),
        (ModelThinkingLevel::Minimal, "minimal"),
        (ModelThinkingLevel::Low, "low"),
        (ModelThinkingLevel::Medium, "medium"),
        (ModelThinkingLevel::High, "high"),
        (ModelThinkingLevel::Xhigh, "xhigh"),
        (ModelThinkingLevel::Max, "max"),
    ] {
        if efforts.contains(&wire) {
            map.insert(level, Some(wire.to_string()));
        } else {
            map.insert(level, None);
        }
    }
    Some(map)
}

fn make_model(id: &str, name: &str, efforts: &[&str], featured: bool) -> Model {
    let context_window = if id.starts_with("claude-") {
        1_000_000
    } else if id.starts_with("gpt-") {
        128_000
    } else {
        1_000_000
    };
    let max_tokens = if id.starts_with("gpt-") {
        32_768
    } else {
        65_536
    };
    Model {
        id: id.to_string(),
        name: name.to_string(),
        api: ANTIGRAVITY_API.to_string(),
        provider: ANTIGRAVITY_PROVIDER.to_string(),
        base_url: "antigravity://local".to_string(),
        reasoning: !efforts.is_empty(),
        thinking_level_map: build_thinking_map(efforts),
        input: vec![ModelInput::Text, ModelInput::Image],
        cost: ModelCost {
            input: JsNumber::from(0.0),
            output: JsNumber::from(0.0),
            cache_read: JsNumber::from(0.0),
            cache_write: JsNumber::from(0.0),
        },
        context_window,
        max_tokens,
        max_tokens_explicit: false,
        featured: featured.then_some(true),
        headers: None,
        compat: None,
        serving: None,
    }
}

/// Instant static catalog used before the first live `agy models` refresh.
#[must_use]
pub fn builtin_models() -> Vec<Model> {
    vec![
        make_model(
            "gemini-4-neon",
            "Gemini 4 Neon",
            &["low", "medium", "high", "xhigh"],
            true,
        ),
        make_model(
            "gemini-4-argon",
            "Gemini 4 Argon",
            &["low", "medium", "high"],
            true,
        ),
        make_model(
            "gemini-4-hydrogen",
            "Gemini 4 Hydrogen",
            &["low", "medium", "high", "xhigh"],
            false,
        ),
        make_model(
            "gemini-pro-orchestrator-neon",
            "Orchestrator (Neon)",
            &[],
            false,
        ),
        make_model(
            "gemini-pro-orchestrator-g1-pro",
            "Orchestrator (Argon)",
            &[],
            false,
        ),
        make_model(
            "gemini-3.8-flash",
            "Gemini 3.8 Flash",
            &["low", "medium", "high"],
            false,
        ),
        make_model(
            "claude-opus-5-5",
            "Claude Opus 5.5",
            &["low", "medium", "high"],
            false,
        ),
        make_model(
            "claude-sonnet-5-5",
            "Claude Sonnet 5.5",
            &["low", "medium", "high"],
            false,
        ),
        make_model("gpt-oss-120b", "GPT-OSS 120B", &["medium"], false),
    ]
}

/// Parse `agy models` output into deduplicated `Model` definitions with per-model
/// `thinking_level_map` entries.
#[must_use]
pub fn parse_models_output(raw: &str) -> Vec<Model> {
    let cleaned = strip_ansi(raw);
    let mut order: Vec<String> = Vec::new();
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut efforts: HashMap<String, Vec<&'static str>> = HashMap::new();

    for line in cleaned.lines() {
        let line = line.rsplit('\r').next().unwrap_or(line).trim();
        if line.is_empty() || line.starts_with("Fetching") || line.starts_with("Usage:") {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let Some(raw_id) = parts.next().filter(|s| !s.is_empty()) else {
            continue;
        };
        if !raw_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
        {
            continue;
        }
        let raw_label = parts.next().map(str::trim).unwrap_or("");
        let (base_id, effort): (String, Option<&'static str>) =
            if let Some((prefix, suffix)) = raw_id.rsplit_once('-') {
                let parsed = match suffix {
                    "low" => Some("low"),
                    "medium" => Some("medium"),
                    "high" => Some("high"),
                    "xhigh" => Some("xhigh"),
                    "max" => Some("max"),
                    _ => None,
                };
                if let Some(level) = parsed {
                    (prefix.to_owned(), Some(level))
                } else {
                    (raw_id.to_owned(), None)
                }
            } else {
                (raw_id.to_owned(), None)
            };
        let base_label = if effort.is_some() {
            raw_label
                .rsplit_once(" (")
                .filter(|(_, tail)| tail.ends_with(')'))
                .map(|(head, _)| head.trim().to_owned())
                .unwrap_or_else(|| raw_label.to_owned())
        } else {
            raw_label.to_owned()
        };
        if !labels.contains_key(&base_id) {
            order.push(base_id.clone());
            labels.insert(
                base_id.clone(),
                if base_label.is_empty() {
                    base_id.clone()
                } else {
                    base_label
                },
            );
        }
        if let Some(level) = effort {
            let list = efforts.entry(base_id).or_default();
            if !list.contains(&level) {
                list.push(level);
            }
        }
    }

    let rank = |s: &str| match s {
        "low" => 0,
        "medium" => 1,
        "high" => 2,
        "xhigh" => 3,
        "max" => 4,
        _ => 5,
    };
    order
        .into_iter()
        .map(|id| {
            let label = labels.remove(&id).unwrap_or_else(|| id.clone());
            let mut levels = efforts.remove(&id).unwrap_or_default();
            levels.sort_by_key(|s| rank(s));
            let featured = matches!(id.as_str(), "gemini-4-neon" | "gemini-4-argon");
            make_model(&id, &label, &levels, featured)
        })
        .collect()
}

/// Return cached live Antigravity models when fresh, otherwise the static fallback
/// (zero-latency for synchronous `ModelRegistry::create`).
#[must_use]
pub fn models() -> Vec<Model> {
    if let Ok(guard) = model_cache().lock() {
        if let Some((_, cached)) = guard.as_ref() {
            if !cached.is_empty() {
                return cached.clone();
            }
        }
    }
    builtin_models()
}

/// Query `<antigravity_bin> models` synchronously and refresh the process-wide cache.
#[must_use]
pub fn refresh_models(force: bool) -> Vec<Model> {
    let Some(bin) = antigravity_bin() else {
        return Vec::new();
    };
    if !force {
        if let Ok(guard) = model_cache().lock() {
            if let Some((fetched_at, cached)) = guard.as_ref() {
                if fetched_at.elapsed() < MODEL_CACHE_TTL && !cached.is_empty() {
                    return cached.clone();
                }
            }
        }
    }
    let output = std::process::Command::new(bin)
        .arg("models")
        .stdin(Stdio::null())
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let parsed = parse_models_output(&String::from_utf8_lossy(&output.stdout));
            if !parsed.is_empty() {
                if let Ok(mut guard) = model_cache().lock() {
                    *guard = Some((Instant::now(), parsed.clone()));
                }
                return parsed;
            }
        }
    }
    models()
}

fn effort_for_level(model: &Model, level: Option<ModelThinkingLevel>) -> Option<&'static str> {
    if !model.reasoning {
        return None;
    }
    let level = level?;
    if let Some(map) = &model.thinking_level_map {
        if let Some(mapped) = map.get(&level) {
            return mapped.as_deref().and_then(|wire| match wire {
                "low" => Some("low"),
                "medium" => Some("medium"),
                "high" => Some("high"),
                "xhigh" => Some("xhigh"),
                "max" => Some("max"),
                _ => None,
            });
        }
    }
    match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal | ModelThinkingLevel::Low => Some("low"),
        ModelThinkingLevel::Medium => Some("medium"),
        ModelThinkingLevel::High => Some("high"),
        ModelThinkingLevel::Xhigh => Some("xhigh"),
        ModelThinkingLevel::Max => Some("max"),
    }
}

fn extract_cwd(context: &Context) -> PathBuf {
    if let Some(system) = &context.system_prompt {
        for line in system.lines() {
            if let Some(rest) = line.strip_prefix("Current working directory:") {
                let path = PathBuf::from(rest.trim());
                if path.is_dir() {
                    return path;
                }
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn user_content_to_text(content: &UserMessageContent) -> String {
    match content {
        UserMessageContent::Text(text) => text.clone(),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match crate::types::user_block_payload(block) {
                crate::types::UserBlockPayload::Text(text) => text.to_string(),
                crate::types::UserBlockPayload::Image { mime_type, .. } => {
                    format!("[attached image: {mime_type}]")
                }
                crate::types::UserBlockPayload::Video { mime_type, .. } => {
                    format!("[attached video: {mime_type}]")
                }
                crate::types::UserBlockPayload::Opaque(json) => json,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn tool_result_to_text(message: &ToolResultMessage) -> String {
    let body = message
        .content
        .iter()
        .map(|block| match crate::types::user_block_payload(block) {
            crate::types::UserBlockPayload::Text(text) => text.to_string(),
            crate::types::UserBlockPayload::Image { mime_type, .. } => {
                format!("[attached image: {mime_type}]")
            }
            crate::types::UserBlockPayload::Video { mime_type, .. } => {
                format!("[attached video: {mime_type}]")
            }
            crate::types::UserBlockPayload::Opaque(json) => json,
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("[tool {} result]: {}", message.tool_name, body)
}

fn message_to_prompt_line(message: &Message) -> Option<String> {
    match message {
        Message::User(user) => {
            let body = user_content_to_text(&user.content);
            (!body.trim().is_empty()).then(|| format!("User: {body}"))
        }
        Message::Assistant(assistant) => {
            let body = assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                        Some(text.text.clone())
                    }
                    AssistantContent::ToolCall(call) => Some(format!(
                        "[called tool {}({})]",
                        call.name,
                        Value::Object(call.arguments.clone())
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!body.trim().is_empty()).then(|| format!("Assistant: {body}"))
        }
        Message::ToolResult(result) => Some(tool_result_to_text(result)),
    }
}

fn build_prompt_and_resume(
    context: &Context,
    model_id: &str,
    session_id: Option<&str>,
) -> (String, Option<String>) {
    let latest_user = context.messages.last().and_then(|msg| match msg {
        Message::User(user) => Some(user_content_to_text(&user.content)),
        _ => None,
    });

    if let (Some(sid), Some(user_text)) = (session_id, &latest_user) {
        if let Ok(guard) = sessions().lock() {
            if let Some(cached) = guard.get(sid) {
                if cached.model_id == model_id && context.messages.len() == cached.message_count + 2
                {
                    return (user_text.clone(), Some(cached.conversation_id.clone()));
                }
            }
        }
    }

    if context.messages.len() == 1 {
        if let Some(user_text) = latest_user {
            return (user_text, None);
        }
    }

    let mut out = String::new();
    let history_end = context
        .messages
        .len()
        .saturating_sub(usize::from(latest_user.is_some()));
    if history_end > 0 {
        out.push_str("Prior conversation context:\n");
        let mut lines = Vec::new();
        let mut budget = 24_000usize;
        for msg in context.messages[..history_end].iter().rev() {
            if let Some(line) = message_to_prompt_line(msg) {
                if line.len() > budget && !lines.is_empty() {
                    break;
                }
                budget = budget.saturating_sub(line.len());
                lines.push(line);
            }
        }
        lines.reverse();
        for line in lines {
            out.push_str(&line);
            out.push_str("\n\n");
        }
    }
    if let Some(user_text) = latest_user {
        if !out.is_empty() {
            out.push_str("Current user request:\n");
        }
        out.push_str(&user_text);
    } else if let Some(last) = context.messages.last().and_then(message_to_prompt_line) {
        out.push_str(&last);
    }
    (out, None)
}

fn empty_assistant_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: Some(Vec::new()),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: now_ms(),
        rest: Map::default(),
    }
}

fn apply_usage(usage: &mut Usage, value: &Value) {
    let input = value["input_tokens"].as_u64().unwrap_or(0);
    let output = value["output_tokens"].as_u64().unwrap_or(0);
    let cache_read = value["cache_read_tokens"].as_u64().unwrap_or(0);
    let total = value["total_tokens"]
        .as_u64()
        .unwrap_or(input + output + cache_read);
    *usage = Usage {
        input,
        output,
        cache_read,
        cache_write: 0,
        total_tokens: total,
        cost: UsageCost::default(),
    };
}

fn push_error(
    writer: &AssistantMessageEventWriter,
    mut partial: AssistantMessage,
    reason: ErrorStopReason,
    message: String,
) {
    partial.stop_reason = match reason {
        ErrorStopReason::Aborted => StopReason::Aborted,
        ErrorStopReason::Error => StopReason::Error,
    };
    partial.error_message = Some(message);
    partial.timestamp = now_ms();
    writer.push(AssistantMessageEvent::Error {
        reason,
        error: partial.clone(),
    });
    writer.end(Some(partial));
}

#[allow(clippy::too_many_lines)]
async fn run_stream(
    writer: AssistantMessageEventWriter,
    model: Model,
    context: Context,
    base: StreamOptions,
    reasoning: Option<ModelThinkingLevel>,
) {
    run_stream_with_bin(writer, model, context, base, reasoning, antigravity_bin()).await;
}

#[allow(clippy::too_many_lines)]
async fn run_stream_with_bin(
    writer: AssistantMessageEventWriter,
    model: Model,
    context: Context,
    base: StreamOptions,
    reasoning: Option<ModelThinkingLevel>,
    bin: Option<PathBuf>,
) {
    let mut partial = empty_assistant_message(&model);
    if base
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        push_error(
            &writer,
            partial,
            ErrorStopReason::Aborted,
            "Request was aborted".to_string(),
        );
        return;
    }

    let Some(bin) = bin else {
        push_error(
            &writer,
            partial,
            ErrorStopReason::Error,
            "Antigravity CLI (agy) is not installed.".to_string(),
        );
        return;
    };

    let cwd = extract_cwd(&context);
    let session_id = base.session_id.clone();
    let (prompt, resume_id) = build_prompt_and_resume(&context, &model.id, session_id.as_deref());
    let effort = effort_for_level(&model, reasoning);

    let mut cmd = Command::new(&bin);
    cmd.args([
        "--output-format",
        "stream-json",
        "--dangerously-skip-permissions",
        "--model",
        &model.id,
    ]);
    if let Some(effort) = effort {
        cmd.args(["--effort", effort]);
    }
    if let Some(cid) = &resume_id {
        cmd.args(["--conversation", cid]);
    }
    cmd.arg(format!("-p={prompt}"))
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            push_error(
                &writer,
                partial,
                ErrorStopReason::Error,
                format!(
                    "Could not start Antigravity CLI ({}): {error}",
                    bin.display()
                ),
            );
            return;
        }
    };

    let Some(stdout) = child.stdout.take() else {
        push_error(
            &writer,
            partial,
            ErrorStopReason::Error,
            "Antigravity CLI stdout unavailable".to_string(),
        );
        return;
    };
    let stderr_handle = child.stderr.take().map(|mut stderr| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).into_owned()
        })
    });

    writer.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut text_started = false;
    let mut text_content_index: u64 = 0;
    let mut conversation_id: Option<String> = resume_id;
    let mut result_text: Option<String> = None;
    let mut result_error: Option<String> = None;

    loop {
        let next_line = if let Some(signal) = &base.signal {
            tokio::select! {
                () = signal.cancelled() => {
                    let _ = child.kill().await;
                    push_error(
                        &writer,
                        partial,
                        ErrorStopReason::Aborted,
                        "Request was aborted".to_string(),
                    );
                    return;
                }
                line = lines.next_line() => line,
            }
        } else {
            lines.next_line().await
        };

        let line = match next_line {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                let _ = child.kill().await;
                push_error(
                    &writer,
                    partial,
                    ErrorStopReason::Error,
                    format!("Failed reading Antigravity output: {error}"),
                );
                return;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        match event["event"].as_str() {
            Some("init") => {
                if let Some(cid) = event["conversation_id"].as_str().filter(|s| !s.is_empty()) {
                    conversation_id = Some(cid.to_string());
                    partial.response_id = Some(cid.to_string());
                }
                if let Some(rm) = event["init"]["model"].as_str().filter(|s| !s.is_empty()) {
                    partial.response_model = Some(rm.to_string());
                }
            }
            Some("step_update") => {
                let update = &event["step_update"];
                if let Some(cid) = update["conversation_id"].as_str().filter(|s| !s.is_empty()) {
                    conversation_id = Some(cid.to_string());
                    partial.response_id = Some(cid.to_string());
                }
                if update["step_type"].as_str() == Some("agent_response") {
                    if let Some(delta) = update["text_delta"].as_str().filter(|s| !s.is_empty()) {
                        if !text_started {
                            text_content_index = partial.content.len() as u64;
                            partial.content.push(AssistantContent::Text(TextContent {
                                text: String::new(),
                                text_signature: None,
                                rest: Map::default(),
                            }));
                            writer.push(AssistantMessageEvent::TextStart {
                                content_index: text_content_index,
                                partial: partial.clone(),
                            });
                            text_started = true;
                        }
                        if let Some(AssistantContent::Text(content)) =
                            partial.content.get_mut(text_content_index as usize)
                        {
                            content.text.push_str(delta);
                        }
                        writer.push(AssistantMessageEvent::TextDelta {
                            content_index: text_content_index,
                            delta: delta.to_string(),
                            partial: partial.clone(),
                        });
                    }
                    if update["usage"].is_object() {
                        apply_usage(&mut partial.usage, &update["usage"]);
                    }
                }
            }
            Some("result") => {
                let res = &event["result"];
                if let Some(cid) = res["conversation_id"].as_str().filter(|s| !s.is_empty()) {
                    conversation_id = Some(cid.to_string());
                    partial.response_id = Some(cid.to_string());
                }
                if res["usage"].is_object() {
                    apply_usage(&mut partial.usage, &res["usage"]);
                }
                if let Some(status) = res["status"].as_str() {
                    if !status.eq_ignore_ascii_case("SUCCESS") {
                        result_error = Some(
                            res["error"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .unwrap_or("Antigravity returned an error")
                                .to_string(),
                        );
                    }
                }
                if let Some(body) = res["response"].as_str().filter(|s| !s.is_empty()) {
                    result_text = Some(body.trim_end_matches('\n').to_string());
                }
            }
            _ => {
                // Single-shot JSON fallback.
                if let Some(status) = event["status"].as_str() {
                    if let Some(cid) = event["conversation_id"].as_str().filter(|s| !s.is_empty()) {
                        conversation_id = Some(cid.to_string());
                        partial.response_id = Some(cid.to_string());
                    }
                    if event["usage"].is_object() {
                        apply_usage(&mut partial.usage, &event["usage"]);
                    }
                    if !status.eq_ignore_ascii_case("SUCCESS") {
                        result_error = Some(
                            event["error"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .unwrap_or("Antigravity returned an error")
                                .to_string(),
                        );
                    } else if let Some(body) = event["response"].as_str() {
                        result_text = Some(body.trim_end_matches('\n').to_string());
                    }
                }
            }
        }
    }

    let status = child.wait().await;
    let stderr_text = match stderr_handle {
        Some(handle) => handle.await.unwrap_or_default(),
        None => String::new(),
    };

    if let Some(error) = result_error {
        push_error(&writer, partial, ErrorStopReason::Error, error);
        return;
    }

    match status {
        Ok(exit) if exit.success() => {}
        Ok(exit) => {
            let detail = stderr_text
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map_or_else(
                    || format!("Antigravity CLI exited with {exit}"),
                    |line| line.trim().to_string(),
                );
            push_error(&writer, partial, ErrorStopReason::Error, detail);
            return;
        }
        Err(error) => {
            push_error(
                &writer,
                partial,
                ErrorStopReason::Error,
                format!("Failed waiting for Antigravity CLI: {error}"),
            );
            return;
        }
    }

    if !text_started {
        if let Some(body) = result_text {
            text_content_index = partial.content.len() as u64;
            partial.content.push(AssistantContent::Text(TextContent {
                text: body.clone(),
                text_signature: None,
                rest: Map::default(),
            }));
            writer.push(AssistantMessageEvent::TextStart {
                content_index: text_content_index,
                partial: partial.clone(),
            });
            writer.push(AssistantMessageEvent::TextDelta {
                content_index: text_content_index,
                delta: body.clone(),
                partial: partial.clone(),
            });
            writer.push(AssistantMessageEvent::TextEnd {
                content_index: text_content_index,
                content: body,
                partial: partial.clone(),
            });
        }
    } else {
        let final_text = if let Some(AssistantContent::Text(content)) =
            partial.content.get_mut(text_content_index as usize)
        {
            let trimmed = content.text.trim_end_matches('\n').to_string();
            content.text.clone_from(&trimmed);
            Some(trimmed)
        } else {
            None
        };
        if let Some(final_text) = final_text {
            writer.push(AssistantMessageEvent::TextEnd {
                content_index: text_content_index,
                content: final_text,
                partial: partial.clone(),
            });
        }
    }

    if let (Some(sid), Some(cid)) = (session_id, conversation_id) {
        if let Ok(mut guard) = sessions().lock() {
            guard.insert(
                sid,
                CachedConversation {
                    conversation_id: cid,
                    model_id: model.id.clone(),
                    message_count: context.messages.len(),
                },
            );
        }
    }

    partial.stop_reason = StopReason::Stop;
    partial.timestamp = now_ms();
    writer.push(AssistantMessageEvent::Done {
        reason: DoneStopReason::Stop,
        message: partial.clone(),
    });
    writer.end(Some(partial));
}

/// Streaming provider implementation backed by the local Antigravity CLI (`agy`).
pub struct AntigravityCliProvider;

impl Provider for AntigravityCliProvider {
    fn api(&self) -> &str {
        ANTIGRAVITY_API
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let (writer, stream) = create_assistant_message_event_stream();
        let model = model.clone();
        let context = context.clone();
        let base = options.cloned().unwrap_or_default();
        tokio::spawn(async move {
            run_stream(writer, model, context, base, None).await;
        });
        stream
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let (writer, stream) = create_assistant_message_event_stream();
        let model = model.clone();
        let context = context.clone();
        let simple = options.cloned().unwrap_or_default();
        tokio::spawn(async move {
            run_stream(writer, model, context, simple.base, simple.reasoning).await;
        });
        stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::get_supported_thinking_levels;

    #[test]
    fn parses_agy_models_with_exact_thinking_maps() {
        let raw = "\r\x1b[32mFetching available models...\x1b[0m\r\
gemini-4-neon-high              Gemini 4 Neon (High)\n\
gemini-4-neon-xhigh             Gemini 4 Neon (XHigh)\n\
gemini-4-neon-medium            Gemini 4 Neon (Medium)\n\
gemini-4-neon-low               Gemini 4 Neon (Low)\n\
gemini-4-argon-high             Gemini 4 Argon (High)\n\
gemini-4-argon-low              Gemini 4 Argon (Low)\n\
gemini-pro-orchestrator-neon    Orchestrator (Neon)\n";
        let parsed = parse_models_output(raw);
        assert_eq!(parsed.len(), 3);
        let neon = &parsed[0];
        assert_eq!(neon.provider, ANTIGRAVITY_PROVIDER);
        assert_eq!(neon.api, ANTIGRAVITY_API);
        assert_eq!(neon.id, "gemini-4-neon");
        assert_eq!(neon.name, "Gemini 4 Neon");
        assert!(neon.reasoning);
        assert_eq!(
            get_supported_thinking_levels(neon),
            vec![
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Xhigh,
            ]
        );
        let orch = &parsed[2];
        assert!(!orch.reasoning);
        assert_eq!(
            get_supported_thinking_levels(orch),
            vec![ModelThinkingLevel::Off]
        );
    }

    #[test]
    fn builtin_catalog_exposes_gemini_4_neon() {
        let catalog = builtin_models();
        let neon = catalog
            .iter()
            .find(|m| m.id == "gemini-4-neon")
            .expect("gemini-4-neon in builtin catalog");
        assert_eq!(neon.provider, "antigravity");
        assert_eq!(neon.api, "antigravity-cli");
        assert_eq!(
            effort_for_level(neon, Some(ModelThinkingLevel::Xhigh)),
            Some("xhigh")
        );
        assert_eq!(
            effort_for_level(neon, Some(ModelThinkingLevel::High)),
            Some("high")
        );
        assert_eq!(effort_for_level(neon, Some(ModelThinkingLevel::Max)), None);
        assert_eq!(effort_for_level(neon, Some(ModelThinkingLevel::Off)), None);

        let orch = catalog
            .iter()
            .find(|m| m.id == "gemini-pro-orchestrator-neon")
            .expect("orchestrator in builtin catalog");
        assert_eq!(effort_for_level(orch, Some(ModelThinkingLevel::High)), None);
    }

    #[test]
    fn builtin_registry_registers_antigravity_cli_provider() {
        crate::registry::ensure_builtins();
        let provider = crate::registry::get_api_provider(ANTIGRAVITY_API);
        assert!(provider.is_some());
        assert_eq!(provider.unwrap().api(), ANTIGRAVITY_API);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streams_text_deltas_usage_and_effort_from_fake_agy() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "pa-antigravity-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("fake-agy");
        let args_log = dir.join("args.txt");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n\
             printf '%s\\n' '{{\"event\":\"init\",\"conversation_id\":\"conv-123\",\"init\":{{\"model\":\"gemini-4-neon-xhigh\"}}}}'\n\
             printf '%s\\n' '{{\"event\":\"step_update\",\"step_update\":{{\"step_type\":\"agent_response\",\"state\":\"ACTIVE\",\"text_delta\":\"Hello \"}}}}'\n\
             printf '%s\\n' '{{\"event\":\"step_update\",\"step_update\":{{\"step_type\":\"agent_response\",\"state\":\"ACTIVE\",\"text_delta\":\"Neon!\"}}}}'\n\
             printf '%s\\n' '{{\"event\":\"result\",\"result\":{{\"conversation_id\":\"conv-123\",\"status\":\"SUCCESS\",\"response\":\"Hello Neon!\",\"usage\":{{\"input_tokens\":12,\"output_tokens\":4,\"cache_read_tokens\":2,\"total_tokens\":18}}}}}}'\n",
            args_log.display()
        );
        std::fs::write(&script_path, script).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let neon = builtin_models()
            .into_iter()
            .find(|m| m.id == "gemini-4-neon")
            .unwrap();
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(pa_types::ai::UserMessage {
                content: UserMessageContent::Text("Say hi".to_string()),
                timestamp: now_ms(),
                rest: Map::default(),
            })],
            tools: None,
        };
        let (writer, stream) = create_assistant_message_event_stream();
        let handle = tokio::spawn(run_stream_with_bin(
            writer,
            neon,
            context,
            StreamOptions {
                session_id: Some("test-session-agy".to_string()),
                ..Default::default()
            },
            Some(ModelThinkingLevel::Xhigh),
            Some(script_path.clone()),
        ));

        let mut deltas = Vec::new();
        let mut stream_iter = stream;
        while let Some(event) = stream_iter.next_event().await {
            if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                deltas.push(delta);
            }
        }
        let message = stream_iter.result().await;
        handle.await.unwrap();

        assert_eq!(deltas, vec!["Hello ", "Neon!"]);
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.response_id.as_deref(), Some("conv-123"));
        assert_eq!(
            message.response_model.as_deref(),
            Some("gemini-4-neon-xhigh")
        );
        assert_eq!(message.usage.input, 12);
        assert_eq!(message.usage.output, 4);
        assert_eq!(message.usage.cache_read, 2);
        assert_eq!(message.usage.total_tokens, 18);

        let recorded_args = std::fs::read_to_string(&args_log).unwrap();
        let args: Vec<&str> = recorded_args.lines().collect();
        assert!(args.windows(2).any(|w| w == ["--model", "gemini-4-neon"]));
        assert!(args.windows(2).any(|w| w == ["--effort", "xhigh"]));
        assert!(args.contains(&"-p=Say hi"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
