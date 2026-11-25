//! Official Claude Code CLI (`claude`) streaming provider for Prime Agent.
//!
//! Routes Anthropic models through the locally installed official `claude` CLI
//! instead of spoofing Claude Code headers against the Messages API with an
//! OAuth subscription token.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::cli_pricing::{apply_estimated_cost, reported_cost_usd};
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStream, AssistantMessageEventWriter,
};
use crate::providers::antigravity::tool_progress_line;
use crate::registry::Provider;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneStopReason,
    ErrorStopReason, Message, Model, ModelThinkingLevel, SimpleStreamOptions, StopReason,
    StreamOptions, TextContent, ThinkingContent, ToolResultMessage, Usage, UsageCost,
    UserMessageContent,
};
use crate::utils_inner::diagnostics::now_ms;

/// Wire API identifier for official Claude Code CLI streaming in `ModelRegistry`.
pub const CLAUDE_CLI_API: &str = "claude-cli";
/// Provider identifier served by Claude Code CLI when no Anthropic API key is configured.
pub const ANTHROPIC_PROVIDER: &str = "anthropic";

#[derive(Clone)]
struct CachedConversation {
    claude_session_id: String,
    model_id: String,
    message_count: usize,
}

fn sessions() -> &'static Mutex<HashMap<String, CachedConversation>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, CachedConversation>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
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

/// Fast filesystem lookup for the official Claude Code CLI executable
/// (`PA_CLAUDE_BIN`, `claude` on `PATH`, `~/.local/bin/claude`, or `~/.claude/local/claude`).
#[must_use]
pub fn claude_bin() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("PA_CLAUDE_BIN") {
        let trimmed = explicit.trim();
        if trimmed.eq_ignore_ascii_case("none") || trimmed == "0" {
            return None;
        }
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Some(found) = find_in_path("claude") {
        return Some(found);
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for rel in [".local/bin/claude", ".claude/local/claude"] {
            let candidate = home.join(rel);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether the official Claude Code CLI executable is installed on this machine.
#[must_use]
pub fn is_available() -> bool {
    claude_bin().is_some()
}

/// Environment variables that make `claude` use an API key instead of the signed-in
/// claude.ai account.
const API_KEY_ENV_VARS: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// Removes inherited API-key variables so a `claude` child process always uses the
/// signed-in claude.ai account, even when the parent shell exports a stale key.
pub fn remove_api_key_env(cmd: &mut std::process::Command) -> &mut std::process::Command {
    for key in API_KEY_ENV_VARS {
        cmd.env_remove(key);
    }
    cmd
}

/// Sign-in state reported by `claude auth status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCliAuthStatus {
    pub logged_in: bool,
    pub auth_method: Option<String>,
    pub subscription_type: Option<String>,
}

/// Parses `claude auth status` JSON. Text around the outermost object is ignored
/// because the CLI can emit a stray byte after the closing brace.
#[must_use]
pub fn parse_auth_status(raw: &str) -> Option<ClaudeCliAuthStatus> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    let value: Value = serde_json::from_str(&raw[start..=end]).ok()?;
    let object = value.as_object()?;
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    Some(ClaudeCliAuthStatus {
        logged_in: object.get("loggedIn").and_then(Value::as_bool)?,
        auth_method: text("authMethod"),
        subscription_type: text("subscriptionType"),
    })
}

/// Runs `claude auth status` without API-key overrides (bounded by `PROBE_TIMEOUT`).
/// Returns `None` when the CLI is missing, times out, fails to run, or prints output
/// that is not a status object.
#[must_use]
pub fn auth_status() -> Option<ClaudeCliAuthStatus> {
    let bin = claude_bin()?;
    let mut cmd = std::process::Command::new(bin);
    remove_api_key_env(&mut cmd).args(["auth", "status"]);
    let output = super::cli_model_probe::run_command_bounded(
        cmd,
        super::cli_model_probe::PROBE_TIMEOUT,
    )?;
    parse_auth_status(&output.stdout)
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

/// The prompt delta for a native resume: everything after the cached turn
/// must be that turn's assistant reply followed only by user rows (the new
/// prompt, plus any user-role context such as `/absorb` appended before
/// it), joined by blank lines.
fn resume_prompt_delta(messages: &[Message], cached_count: usize) -> Option<String> {
    let (reply, appended) = messages.get(cached_count..)?.split_first()?;
    if !matches!(reply, Message::Assistant(_)) || appended.is_empty() {
        return None;
    }
    let texts = appended
        .iter()
        .map(|message| match message {
            Message::User(user) => Some(user_content_to_text(&user.content)),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(
        texts
            .into_iter()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
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

    if let (Some(sid), Some(_)) = (session_id, &latest_user) {
        if let Ok(guard) = sessions().lock() {
            if let Some(cached) = guard.get(sid) {
                if cached.model_id == model_id {
                    if let Some(delta) =
                        resume_prompt_delta(&context.messages, cached.message_count)
                    {
                        return (delta, Some(cached.claude_session_id.clone()));
                    }
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

fn apply_usage(model: &Model, response_model: Option<&str>, usage: &mut Usage, value: &Value) {
    let input = value["input_tokens"].as_u64().unwrap_or(usage.input);
    let output = value["output_tokens"].as_u64().unwrap_or(usage.output);
    let cache_read = value["cache_read_input_tokens"]
        .as_u64()
        .unwrap_or(usage.cache_read);
    let cache_write = value["cache_creation_input_tokens"]
        .as_u64()
        .unwrap_or(usage.cache_write);
    let total = input + output + cache_read + cache_write;
    *usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: total,
        cost: UsageCost::default(),
    };
    apply_estimated_cost(model, response_model, usage, None);
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

fn push_thinking(
    chunk: &str,
    thinking_index: &mut Option<u64>,
    partial: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
) {
    let idx = if let Some(idx) = *thinking_index {
        idx
    } else {
        let idx = partial.content.len() as u64;
        partial
            .content
            .push(AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: None,
                redacted: None,
                rest: Map::default(),
            }));
        writer.push(AssistantMessageEvent::ThinkingStart {
            content_index: idx,
            partial: partial.clone(),
        });
        *thinking_index = Some(idx);
        idx
    };
    if let Some(AssistantContent::Thinking(content)) = partial.content.get_mut(idx as usize) {
        content.thinking.push_str(chunk);
    }
    writer.push(AssistantMessageEvent::ThinkingDelta {
        content_index: idx,
        delta: chunk.to_string(),
        partial: partial.clone(),
    });
}

pub(crate) async fn run_stream(
    writer: AssistantMessageEventWriter,
    model: Model,
    context: Context,
    base: StreamOptions,
    reasoning: Option<ModelThinkingLevel>,
) {
    run_stream_with_bin(writer, model, context, base, reasoning, claude_bin()).await;
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
            "Claude Code CLI (claude) is not installed.".to_string(),
        );
        return;
    };

    let cwd = extract_cwd(&context);
    let session_id = base.session_id.clone();
    let (prompt, resume_id) = build_prompt_and_resume(&context, &model.id, session_id.as_deref());
    let effort = effort_for_level(&model, reasoning);

    let mut cmd = Command::new(&bin);
    cmd.args([
        "--print",
        "--verbose",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--dangerously-skip-permissions",
        "--model",
        &model.id,
    ]);
    if let Some(effort) = effort {
        cmd.args(["--effort", effort]);
    }
    if let Some(cid) = &resume_id {
        cmd.args(["--resume", cid]);
    }
    remove_api_key_env(cmd.as_std_mut());
    cmd.current_dir(&cwd)
        .stdin(Stdio::piped())
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
                    "Could not start Claude Code CLI ({}): {error}",
                    bin.display()
                ),
            );
            return;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let prompt_bytes = prompt.into_bytes();
        tokio::spawn(async move {
            let _ = stdin.write_all(&prompt_bytes).await;
            let _ = stdin.shutdown().await;
        });
    }

    let Some(stdout) = child.stdout.take() else {
        push_error(
            &writer,
            partial,
            ErrorStopReason::Error,
            "Claude Code CLI stdout unavailable".to_string(),
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
    let mut thinking_index: Option<u64> = None;
    let mut text_index: Option<u64> = None;
    let mut claude_session_id: Option<String> = resume_id;
    let mut result_text: Option<String> = None;
    let mut result_error: Option<String> = None;
    // Streamed `tool_use` blocks by content index: (id, name, partial input JSON).
    let mut pending_tools: HashMap<String, (String, String, String)> = HashMap::new();
    let mut seen_tool_ids: HashSet<String> = HashSet::new();

    let close_thinking = |thinking_index: &mut Option<u64>,
                          partial: &mut AssistantMessage,
                          writer: &AssistantMessageEventWriter| {
        if let Some(idx) = thinking_index.take() {
            let final_thinking = if let Some(AssistantContent::Thinking(content)) =
                partial.content.get(idx as usize)
            {
                Some(content.thinking.clone())
            } else {
                None
            };
            if let Some(content) = final_thinking {
                writer.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: idx,
                    content,
                    partial: partial.clone(),
                });
            }
        }
    };

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
                    format!("Failed reading Claude Code output: {error}"),
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

        match event["type"].as_str() {
            Some("system") => {
                if let Some(sid) = event["session_id"].as_str().filter(|s| !s.is_empty()) {
                    claude_session_id = Some(sid.to_string());
                    partial.response_id = Some(sid.to_string());
                }
                if let Some(rm) = event["model"].as_str().filter(|s| !s.is_empty()) {
                    partial.response_model = Some(rm.to_string());
                }
            }
            Some("stream_event") => {
                let inner = &event["event"];
                match inner["type"].as_str() {
                    Some("message_start") => {
                        let msg = &inner["message"];
                        if let Some(rm) = msg["model"].as_str().filter(|s| !s.is_empty()) {
                            partial.response_model = Some(rm.to_string());
                        }
                        if msg["usage"].is_object() {
                            apply_usage(
                                &model,
                                partial.response_model.as_deref(),
                                &mut partial.usage,
                                &msg["usage"],
                            );
                        }
                    }
                    Some("content_block_delta") => {
                        let delta = &inner["delta"];
                        match delta["type"].as_str() {
                            Some("thinking_delta") => {
                                if let Some(chunk) =
                                    delta["thinking"].as_str().filter(|s| !s.is_empty())
                                {
                                    push_thinking(
                                        chunk,
                                        &mut thinking_index,
                                        &mut partial,
                                        &writer,
                                    );
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some(pending) =
                                    pending_tools.get_mut(&inner["index"].to_string())
                                {
                                    pending.2.push_str(
                                        delta["partial_json"].as_str().unwrap_or_default(),
                                    );
                                }
                            }
                            Some("text_delta") => {
                                if let Some(chunk) =
                                    delta["text"].as_str().filter(|s| !s.is_empty())
                                {
                                    close_thinking(&mut thinking_index, &mut partial, &writer);
                                    let idx = match text_index {
                                        Some(idx) => idx,
                                        None => {
                                            let idx = partial.content.len() as u64;
                                            partial.content.push(AssistantContent::Text(
                                                TextContent {
                                                    text: String::new(),
                                                    text_signature: None,
                                                    rest: Map::default(),
                                                },
                                            ));
                                            writer.push(AssistantMessageEvent::TextStart {
                                                content_index: idx,
                                                partial: partial.clone(),
                                            });
                                            text_index = Some(idx);
                                            idx
                                        }
                                    };
                                    if let Some(AssistantContent::Text(content)) =
                                        partial.content.get_mut(idx as usize)
                                    {
                                        content.text.push_str(chunk);
                                    }
                                    writer.push(AssistantMessageEvent::TextDelta {
                                        content_index: idx,
                                        delta: chunk.to_string(),
                                        partial: partial.clone(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("content_block_start") => {
                        let block = &inner["content_block"];
                        if block["type"].as_str() == Some("tool_use") {
                            pending_tools.insert(
                                inner["index"].to_string(),
                                (
                                    block["id"].as_str().unwrap_or_default().to_string(),
                                    block["name"].as_str().unwrap_or("tool").to_string(),
                                    String::new(),
                                ),
                            );
                        }
                    }
                    Some("content_block_stop") => {
                        if let Some((id, name, raw_input)) =
                            pending_tools.remove(&inner["index"].to_string())
                        {
                            if id.is_empty() || seen_tool_ids.insert(id) {
                                let input = serde_json::from_str::<Value>(&raw_input)
                                    .unwrap_or(Value::Null);
                                push_thinking(
                                    &tool_progress_line(&name, &input),
                                    &mut thinking_index,
                                    &mut partial,
                                    &writer,
                                );
                            }
                        }
                    }
                    Some("message_delta") => {
                        if inner["usage"].is_object() {
                            apply_usage(
                                &model,
                                partial.response_model.as_deref(),
                                &mut partial.usage,
                                &inner["usage"],
                            );
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                let msg = &event["message"];
                if let Some(rm) = msg["model"].as_str().filter(|s| !s.is_empty()) {
                    partial.response_model = Some(rm.to_string());
                }
                if msg["usage"].is_object() {
                    apply_usage(
                        &model,
                        partial.response_model.as_deref(),
                        &mut partial.usage,
                        &msg["usage"],
                    );
                }
                if let Some(blocks) = msg["content"].as_array() {
                    for block in blocks {
                        if block["type"].as_str() != Some("tool_use") {
                            continue;
                        }
                        let id = block["id"].as_str().unwrap_or_default();
                        if id.is_empty() || seen_tool_ids.insert(id.to_string()) {
                            push_thinking(
                                &tool_progress_line(
                                    block["name"].as_str().unwrap_or("tool"),
                                    &block["input"],
                                ),
                                &mut thinking_index,
                                &mut partial,
                                &writer,
                            );
                        }
                    }
                    let combined: String = blocks
                        .iter()
                        .filter_map(|b| {
                            (b["type"].as_str() == Some("text"))
                                .then(|| b["text"].as_str())
                                .flatten()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !combined.is_empty() {
                        result_text = Some(combined);
                    }
                }
            }
            Some("result") => {
                if let Some(sid) = event["session_id"].as_str().filter(|s| !s.is_empty()) {
                    claude_session_id = Some(sid.to_string());
                    partial.response_id = Some(sid.to_string());
                }
                if event["usage"].is_object() {
                    apply_usage(
                        &model,
                        partial.response_model.as_deref(),
                        &mut partial.usage,
                        &event["usage"],
                    );
                }
                apply_estimated_cost(
                    &model,
                    partial.response_model.as_deref(),
                    &mut partial.usage,
                    reported_cost_usd(&event),
                );
                if event["is_error"].as_bool() == Some(true) {
                    result_error = Some(
                        event["result"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or_else(|| {
                                event["errors"]
                                    .as_array()
                                    .and_then(|arr| arr.first())
                                    .and_then(Value::as_str)
                            })
                            .unwrap_or("Claude Code returned an error")
                            .to_string(),
                    );
                } else if let Some(body) = event["result"].as_str().filter(|s| !s.is_empty()) {
                    result_text = Some(body.to_string());
                }
            }
            _ => {}
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
                    || format!("Claude Code CLI exited with {exit}"),
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
                format!("Failed waiting for Claude Code CLI: {error}"),
            );
            return;
        }
    }

    close_thinking(&mut thinking_index, &mut partial, &writer);

    if let Some(idx) = text_index {
        let final_text =
            if let Some(AssistantContent::Text(content)) = partial.content.get_mut(idx as usize) {
                let trimmed = content.text.trim_end_matches('\n').to_string();
                content.text.clone_from(&trimmed);
                Some(trimmed)
            } else {
                None
            };
        if let Some(final_text) = final_text {
            writer.push(AssistantMessageEvent::TextEnd {
                content_index: idx,
                content: final_text,
                partial: partial.clone(),
            });
        }
    } else if let Some(body) = result_text {
        let idx = partial.content.len() as u64;
        partial.content.push(AssistantContent::Text(TextContent {
            text: body.clone(),
            text_signature: None,
            rest: Map::default(),
        }));
        writer.push(AssistantMessageEvent::TextStart {
            content_index: idx,
            partial: partial.clone(),
        });
        writer.push(AssistantMessageEvent::TextDelta {
            content_index: idx,
            delta: body.clone(),
            partial: partial.clone(),
        });
        writer.push(AssistantMessageEvent::TextEnd {
            content_index: idx,
            content: body,
            partial: partial.clone(),
        });
    }

    if let (Some(sid), Some(cid)) = (session_id, claude_session_id) {
        if let Ok(mut guard) = sessions().lock() {
            guard.insert(
                sid,
                CachedConversation {
                    claude_session_id: cid,
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

/// Streaming provider backed by the official Claude Code CLI (`claude`).
pub struct ClaudeCliProvider;

impl Provider for ClaudeCliProvider {
    fn api(&self) -> &str {
        CLAUDE_CLI_API
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

    #[test]
    fn absorbed_context_between_turns_keeps_native_resume() {
        let text_user = |text: &str| {
            Message::User(pa_types::ai::UserMessage {
                content: UserMessageContent::Text(text.to_string()),
                timestamp: now_ms(),
                rest: Map::default(),
            })
        };
        let reply = Message::Assistant(AssistantMessage {
            content: Vec::new(),
            api: String::new(),
            provider: String::new(),
            model: "resume-model".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: now_ms(),
            rest: Map::default(),
        });
        let sid = "test-session-claude_cli-absorb-resume";
        sessions().lock().unwrap().insert(
            sid.to_string(),
            CachedConversation {
                claude_session_id: "native-conv-1".into(),
                model_id: "resume-model".into(),
                message_count: 1,
            },
        );
        let context = Context {
            messages: vec![
                text_user("turn one"),
                reply.clone(),
                text_user("[absorbed-context]"),
                text_user("turn two"),
            ],
            ..Default::default()
        };
        let (prompt, resume) = build_prompt_and_resume(&context, "resume-model", Some(sid));
        assert_eq!(resume.as_deref(), Some("native-conv-1"));
        assert_eq!(prompt, "[absorbed-context]\n\nturn two");

        let interleaved = Context {
            messages: vec![
                text_user("turn one"),
                reply.clone(),
                reply,
                text_user("turn two"),
            ],
            ..Default::default()
        };
        let (_, resume) = build_prompt_and_resume(&interleaved, "resume-model", Some(sid));
        assert_eq!(resume, None, "an extra assistant row breaks the resume");
        sessions().lock().unwrap().remove(sid);
    }
    use crate::types::{zero_model_cost, ModelInput, UserMessage};

    fn opus_model() -> Model {
        Model {
            id: "claude-opus-5-5".into(),
            name: "Claude Opus 5.5".into(),
            api: CLAUDE_CLI_API.into(),
            provider: ANTHROPIC_PROVIDER.into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text, ModelInput::Image],
            cost: zero_model_cost(),
            context_window: 1_000_000,
            max_tokens: 64_000,
            max_tokens_explicit: false,
            featured: Some(true),
            headers: None,
            compat: None,
            serving: None,
        }
    }

    #[test]
    fn parses_auth_status_with_trailing_noise() {
        let raw = "{\n  \"loggedIn\": true,\n  \"authMethod\": \"claude.ai\",\n  \"apiProvider\": \"firstParty\",\n  \"subscriptionType\": \"team\"\n}\u{1b}";
        assert_eq!(
            parse_auth_status(raw),
            Some(ClaudeCliAuthStatus {
                logged_in: true,
                auth_method: Some("claude.ai".to_string()),
                subscription_type: Some("team".to_string()),
            })
        );
        assert_eq!(
            parse_auth_status("{\"loggedIn\": false}\n%"),
            Some(ClaudeCliAuthStatus {
                logged_in: false,
                auth_method: None,
                subscription_type: None,
            })
        );
    }

    #[test]
    fn rejects_auth_status_without_status_object() {
        assert_eq!(parse_auth_status(""), None);
        assert_eq!(parse_auth_status("Not logged in"), None);
        assert_eq!(parse_auth_status("{\"authMethod\": \"claude.ai\"}"), None);
        assert_eq!(parse_auth_status("} {"), None);
    }

    #[test]
    fn removes_api_key_env_from_claude_commands() {
        let mut cmd = std::process::Command::new("claude");
        cmd.env("ANTHROPIC_API_KEY", "sk-ant-stale");
        remove_api_key_env(&mut cmd);
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert_eq!(removed, vec!["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]);
    }

    #[test]
    fn builtin_registry_registers_claude_cli_provider() {
        crate::registry::ensure_builtins();
        let provider = crate::registry::get_api_provider(CLAUDE_CLI_API);
        assert!(provider.is_some());
        assert_eq!(provider.unwrap().api(), CLAUDE_CLI_API);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streams_thinking_text_usage_and_effort_from_fake_claude() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "pa-claude-cli-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("fake-claude");
        let args_log = dir.join("args.txt");
        let stdin_log = dir.join("stdin.txt");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\ncat > \"{}\"\n\
             printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"cc-sess-1\",\"model\":\"claude-opus-5-5\"}}'\n\
             printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"Plan: \"}}}}}}'\n\
             printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{{}}}}}}}}'\n\
             printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"command\\\":\\\"cargo \"}}}}}}'\n\
             printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"test\\\"}}\"}}}}}}'\n\
             printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_stop\",\"index\":1}}}}'\n\
             printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{{\"command\":\"cargo test\"}}}},{{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"Read\",\"input\":{{\"file_path\":\"/tmp/a.rs\"}}}}]}}}}'\n\
             printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"Hello \"}}}}}}'\n\
             printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"Claude!\"}}}}}}'\n\
             printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"cc-sess-1\",\"result\":\"Hello Claude!\",\"usage\":{{\"input_tokens\":20,\"output_tokens\":5,\"cache_read_input_tokens\":10,\"cache_creation_input_tokens\":3}}}}'\n",
            args_log.display(),
            stdin_log.display()
        );
        std::fs::write(&script_path, script).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserMessageContent::Text("Say hi".to_string()),
                timestamp: now_ms(),
                rest: Map::default(),
            })],
            tools: None,
        };
        let (writer, mut stream) = create_assistant_message_event_stream();
        let handle = tokio::spawn(run_stream_with_bin(
            writer,
            opus_model(),
            context,
            StreamOptions {
                session_id: Some("test-session-cc".to_string()),
                ..Default::default()
            },
            Some(ModelThinkingLevel::High),
            Some(script_path),
        ));

        let mut thinking_deltas = Vec::new();
        let mut text_deltas = Vec::new();
        while let Some(event) = stream.next_event().await {
            match event {
                AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking_deltas.push(delta),
                AssistantMessageEvent::TextDelta { delta, .. } => text_deltas.push(delta),
                _ => {}
            }
        }
        let message = stream.result().await;
        handle.await.unwrap();

        assert_eq!(
            thinking_deltas,
            vec!["Plan: ", "\n▸ Bash cargo test\n", "\n▸ Read /tmp/a.rs\n"]
        );
        assert_eq!(text_deltas, vec!["Hello ", "Claude!"]);
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.response_id.as_deref(), Some("cc-sess-1"));
        assert_eq!(message.response_model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(message.usage.input, 20);
        assert_eq!(message.usage.output, 5);
        assert_eq!(message.usage.cache_read, 10);
        assert_eq!(message.usage.cache_write, 3);
        assert_eq!(message.usage.total_tokens, 38);
        // No reported total: Opus list price ($5 in, $25 out, $0.50 read, $6.25 write per 1M).
        assert!((message.usage.cost.total.as_f64() - 248.75e-6).abs() < 1e-12);

        let recorded_args = std::fs::read_to_string(&args_log).unwrap();
        let args: Vec<&str> = recorded_args.lines().collect();
        assert!(args.windows(2).any(|w| w == ["--model", "claude-opus-5-5"]));
        assert!(args.windows(2).any(|w| w == ["--effort", "high"]));
        assert_eq!(std::fs::read_to_string(&stdin_log).unwrap(), "Say hi");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn result_total_cost_usd_overrides_estimate_and_keeps_tokens() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "pa-claude-cli-cost-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("fake-claude");
        let script = "#!/bin/sh\ncat > /dev/null\n\
             printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"cc-sess-2\",\"model\":\"claude-opus-5-5\"}'\n\
             printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"model\":\"claude-opus-5-5\",\"content\":[{\"type\":\"text\",\"text\":\"Hi\"}],\"usage\":{\"input_tokens\":1000,\"output_tokens\":100}}}'\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"cc-sess-2\",\"result\":\"Hi\",\"total_cost_usd\":0.0123}'\n";
        std::fs::write(&script_path, script).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                content: UserMessageContent::Text("Say hi".to_string()),
                timestamp: now_ms(),
                rest: Map::default(),
            })],
            tools: None,
        };
        let (writer, mut stream) = create_assistant_message_event_stream();
        let handle = tokio::spawn(run_stream_with_bin(
            writer,
            opus_model(),
            context,
            StreamOptions::default(),
            None,
            Some(script_path),
        ));
        while stream.next_event().await.is_some() {}
        let message = stream.result().await;
        handle.await.unwrap();

        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.usage.input, 1000);
        assert_eq!(message.usage.output, 100);
        assert!((message.usage.cost.input.as_f64() - 5e-3).abs() < 1e-12);
        assert!((message.usage.cost.output.as_f64() - 2.5e-3).abs() < 1e-12);
        assert_eq!(message.usage.cost.total.as_f64(), 0.0123);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
