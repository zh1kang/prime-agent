//! Cursor CLI (`cursor-agent` / `agent`) streaming provider for Prime Agent.
//!
//! Surfaces the models advertised by `cursor-agent --list-models` (Claude Opus
//! 5.5, Composer, Grok, Gemini, GPT, ...) directly in `ModelRegistry` and streams
//! completions through `cursor-agent --print --output-format stream-json`.
//! Cursor encodes effort (and fast mode) in the model ID itself, so effort
//! variants are grouped into one base model whose `thinking_level_map` stores
//! the exact advertised wire ID per level.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::cli_pricing::{apply_estimated_cost, list_price_for_model, reported_cost_usd};
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStream, AssistantMessageEventWriter,
};
use crate::providers::antigravity::tool_progress_line;
use crate::providers::cli_model_probe as probe;
use crate::registry::Provider;
use crate::types::{
    zero_model_cost, AssistantContent, AssistantMessage, AssistantMessageEvent, Context,
    DoneStopReason, ErrorStopReason, Message, Model, ModelInput, ModelThinkingLevel,
    SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingContent, ThinkingLevelMap,
    ToolResultMessage, Usage, UsageCost, UserMessageContent,
};
use crate::utils_inner::diagnostics::now_ms;

/// Wire API identifier for Cursor CLI models in `ModelRegistry`.
pub const CURSOR_CLI_API: &str = "cursor-cli";
/// Provider identifier for Cursor models in `ModelRegistry`.
pub const CURSOR_PROVIDER: &str = "cursor";
/// Placeholder base URL for Cursor CLI models (requests never leave the CLI).
pub const CURSOR_BASE_URL: &str = "cursor://local";

const MODEL_CACHE_TTL: Duration = Duration::from_secs(300);
/// The cross-process copy of the last `--list-models` listing, under `<agent dir>/cache/`.
const MODEL_DISK_CACHE: &str = "cursor-models.json";

const ALL_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

#[derive(Clone)]
struct CachedChat {
    chat_id: String,
    model_id: String,
    message_count: usize,
}

fn sessions() -> &'static Mutex<HashMap<String, CachedChat>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, CachedChat>>> = OnceLock::new();
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

/// Fast filesystem lookup for the Cursor CLI executable (`PA_CURSOR_BIN`,
/// `cursor-agent` on `PATH`, `~/.local/bin/cursor-agent`, or `~/.local/bin/agent`).
///
/// A generic `agent` on `PATH` is deliberately not considered: unrelated tools
/// ship binaries with that name.
#[must_use]
pub fn cursor_bin() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("PA_CURSOR_BIN") {
        let trimmed = explicit.trim();
        if trimmed.eq_ignore_ascii_case("none") || trimmed == "0" {
            return None;
        }
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Some(found) = find_in_path("cursor-agent") {
        return Some(found);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    [".local/bin/cursor-agent", ".local/bin/agent"]
        .into_iter()
        .map(|rel| home.join(rel))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether a Cursor CLI executable is installed on this machine.
#[must_use]
pub fn is_available() -> bool {
    cursor_bin().is_some()
}

/// Sign-in state reported by `cursor-agent status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorCliAuthStatus {
    pub logged_in: bool,
    pub account: Option<String>,
    pub team: Option<String>,
}

/// Parse `cursor-agent status` output (ANSI-stripped).
#[must_use]
pub fn parse_auth_status(raw: &str) -> Option<CursorCliAuthStatus> {
    let cleaned = clean_output(raw);
    let mut logged_in = false;
    let mut account = None;
    let mut team = None;
    for line in cleaned.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("logged in as ") {
            logged_in = true;
            let rest = line[idx + "logged in as ".len()..].trim();
            if !rest.is_empty() {
                account = Some(rest.to_string());
            }
        } else if lower.contains("not logged in") {
            return Some(CursorCliAuthStatus {
                logged_in: false,
                account: None,
                team: None,
            });
        } else if let Some(rest) = line.strip_prefix("Team:").or_else(|| line.strip_prefix("team:")) {
            let trimmed = rest.trim();
            let name = trimmed
                .split(" (id ")
                .next()
                .map(str::trim)
                .unwrap_or(trimmed);
            if !name.is_empty() {
                team = Some(name.to_string());
            }
        }
    }
    if logged_in {
        Some(CursorCliAuthStatus {
            logged_in: true,
            account,
            team,
        })
    } else {
        None
    }
}

/// Query `cursor-agent status` synchronously with a short timeout.
#[must_use]
pub fn auth_status() -> Option<CursorCliAuthStatus> {
    let bin = cursor_bin()?;
    let output = probe::run_bounded(&bin, &["status"], Duration::from_secs(4))?;
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    parse_auth_status(&combined)
}

fn clean_output(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
            }
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}' => {}
            _ => out.push(c),
        }
    }
    out
}

fn level_for_suffix(suffix: &str) -> Option<ModelThinkingLevel> {
    match suffix {
        "none" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" | "extra-high" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

/// Split an advertised wire ID into its grouped base ID and effort level:
/// `claude-opus-5-5-high-fast` -> (`claude-opus-5-5-fast`, High),
/// `claude-4.6-opus-max-thinking` -> (`claude-4.6-opus-thinking`, Max),
/// `composer-2.5` -> (`composer-2.5`, None).
fn split_wire_id(wire: &str) -> (String, Option<ModelThinkingLevel>) {
    let (rest, fast) = wire
        .strip_suffix("-fast")
        .map_or((wire, false), |stem| (stem, true));
    let (rest, thinking) = rest
        .strip_suffix("-thinking")
        .map_or((rest, false), |stem| (stem, true));
    let (stem, level) = rest
        .strip_suffix("-extra-high")
        .map(|stem| (stem, Some(ModelThinkingLevel::Xhigh)))
        .or_else(|| {
            rest.rsplit_once('-').and_then(|(stem, suffix)| {
                level_for_suffix(suffix).map(|level| (stem, Some(level)))
            })
        })
        .unwrap_or((rest, None));
    let mut base = stem.to_string();
    if thinking {
        base.push_str("-thinking");
    }
    if fast {
        base.push_str("-fast");
    }
    (base, level)
}

fn base_label(label: &str) -> String {
    let words: Vec<&str> = label.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        if word == "Extra" && words.get(i + 1) == Some(&"High") {
            i += 2;
            continue;
        }
        // "No Thinking" / "Low Thinking" keep their meaning only with the word before.
        let next_is_thinking = words.get(i + 1) == Some(&"Thinking") && word == "No";
        if !next_is_thinking
            && matches!(word, "None" | "Minimal" | "Low" | "Medium" | "High" | "Max")
        {
            i += 1;
            continue;
        }
        out.push(word);
        i += 1;
    }
    out.join(" ")
}

fn make_model(id: &str, name: &str, wires: &[(ModelThinkingLevel, &str)]) -> Model {
    let thinking_level_map: Option<ThinkingLevelMap> = (!wires.is_empty()).then(|| {
        let mut map = BTreeMap::new();
        for level in ALL_LEVELS {
            let wire = wires
                .iter()
                .find(|(l, _)| *l == level)
                .map(|(_, w)| (*w).to_string());
            map.insert(level, wire);
        }
        map
    });
    let context_window = if name.contains("1M") || id.starts_with("gemini-") {
        1_000_000
    } else {
        200_000
    };
    Model {
        id: id.to_string(),
        name: name.to_string(),
        api: CURSOR_CLI_API.to_string(),
        provider: CURSOR_PROVIDER.to_string(),
        base_url: CURSOR_BASE_URL.to_string(),
        reasoning: !wires.is_empty(),
        thinking_level_map,
        input: vec![ModelInput::Text, ModelInput::Image],
        cost: list_price_for_model(id).unwrap_or_else(zero_model_cost),
        context_window,
        max_tokens: 64_000,
        max_tokens_explicit: false,
        featured: matches!(id, "claude-opus-5-5" | "composer-2.5").then_some(true),
        headers: None,
        compat: None,
        serving: None,
    }
}

fn effort_family(base: &str, levels: &[ModelThinkingLevel]) -> Vec<(ModelThinkingLevel, String)> {
    levels
        .iter()
        .map(|level| {
            let suffix = match level {
                ModelThinkingLevel::Off => "none",
                ModelThinkingLevel::Minimal => "minimal",
                ModelThinkingLevel::Low => "low",
                ModelThinkingLevel::Medium => "medium",
                ModelThinkingLevel::High => "high",
                ModelThinkingLevel::Xhigh => "xhigh",
                ModelThinkingLevel::Max => "max",
            };
            let wire = match base.strip_suffix("-fast") {
                Some(stem) => format!("{stem}-{suffix}-fast"),
                None => format!("{base}-{suffix}"),
            };
            (*level, wire)
        })
        .collect()
}

fn make_family(id: &str, name: &str, levels: &[ModelThinkingLevel]) -> Model {
    let wires = effort_family(id, levels);
    let refs: Vec<(ModelThinkingLevel, &str)> =
        wires.iter().map(|(l, w)| (*l, w.as_str())).collect();
    make_model(id, name, &refs)
}

/// Instant static catalog used before the first live `cursor-agent --list-models` refresh.
#[must_use]
pub fn builtin_models() -> Vec<Model> {
    use ModelThinkingLevel::{High, Low, Max, Medium, Xhigh};
    vec![
        make_model("auto", "Auto", &[]),
        make_family(
            "claude-opus-5-5",
            "Claude Opus 5.5 1M",
            &[Low, Medium, High, Xhigh, Max],
        ),
        make_family(
            "claude-opus-5-5-fast",
            "Claude Opus 5.5 1M Fast",
            &[Low, Medium, High, Xhigh, Max],
        ),
        make_model("composer-2.5", "Composer 2.5", &[]),
        make_model("composer-2.5-fast", "Composer 2.5 Fast", &[]),
        make_family("grok-4.7", "Grok 4.7", &[Low, Medium, High, Xhigh]),
        make_family(
            "gemini-4-neon",
            "Gemini 4 Neon",
            &[Low, Medium, High, Xhigh],
        ),
    ]
}

/// Parse `cursor-agent --list-models` output into grouped `Model` definitions
/// whose `thinking_level_map` stores the exact advertised wire ID per level.
#[must_use]
pub fn parse_models_output(raw: &str) -> Vec<Model> {
    struct Group {
        label: String,
        label_from_bare: bool,
        bare: Option<String>,
        wires: Vec<(ModelThinkingLevel, String)>,
    }

    let cleaned = clean_output(raw);
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Group> = HashMap::new();

    for line in cleaned.lines() {
        let line = line.rsplit('\r').next().unwrap_or(line).trim();
        let Some((raw_id, raw_label)) = line.split_once(" - ") else {
            continue;
        };
        let wire = raw_id.trim();
        if wire.is_empty()
            || !wire
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        {
            continue;
        }
        let raw_label = raw_label
            .trim()
            .trim_end_matches("(default)")
            .trim()
            .to_string();
        let (base, level) = split_wire_id(wire);
        let group = groups.entry(base.clone()).or_insert_with(|| {
            order.push(base.clone());
            Group {
                label: String::new(),
                label_from_bare: false,
                bare: None,
                wires: Vec::new(),
            }
        });
        match level {
            Some(level) => {
                if !group.wires.iter().any(|(l, _)| *l == level) {
                    group.wires.push((level, wire.to_string()));
                }
                if group.label.is_empty() {
                    group.label = base_label(&raw_label);
                }
            }
            None => {
                group.bare = Some(wire.to_string());
                if !group.label_from_bare {
                    group.label = raw_label.split_whitespace().collect::<Vec<_>>().join(" ");
                    group.label_from_bare = true;
                }
            }
        }
    }

    order
        .into_iter()
        .filter_map(|id| {
            let mut group = groups.remove(&id)?;
            if let Some(bare) = group.bare.take() {
                if !group.wires.is_empty()
                    && !group
                        .wires
                        .iter()
                        .any(|(l, _)| *l == ModelThinkingLevel::Medium)
                {
                    group.wires.push((ModelThinkingLevel::Medium, bare));
                }
            }
            group.wires.sort_by_key(|(level, _)| *level);
            let label = if group.label.is_empty() {
                id.clone()
            } else {
                group.label
            };
            let refs: Vec<(ModelThinkingLevel, &str)> =
                group.wires.iter().map(|(l, w)| (*l, w.as_str())).collect();
            Some(make_model(&id, &label, &refs))
        })
        .collect()
}

/// Return cached live Cursor models when fresh, otherwise the static fallback
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

/// Query `<cursor_bin> --list-models` synchronously and refresh the process-wide cache.
#[must_use]
pub fn refresh_models(force: bool) -> Vec<Model> {
    let Some(bin) = cursor_bin() else {
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
    let disk_cache = probe::cache_path(MODEL_DISK_CACHE);
    if !force {
        if let Some((stdout, age)) = disk_cache
            .as_deref()
            .and_then(|path| probe::read_cached(path, &bin, MODEL_CACHE_TTL))
        {
            let parsed = parse_models_output(&stdout);
            if !parsed.is_empty() {
                if let Ok(mut guard) = model_cache().lock() {
                    *guard = Some((probe::fetched_instant(age), parsed.clone()));
                }
                return parsed;
            }
        }
    }
    let timeout = if force {
        probe::FORCE_PROBE_TIMEOUT
    } else {
        probe::PROBE_TIMEOUT
    };
    if let Some(output) = probe::run_bounded(&bin, &["--list-models"], timeout) {
        if output.success {
            let parsed = parse_models_output(&output.stdout);
            if !parsed.is_empty() {
                if let Ok(mut guard) = model_cache().lock() {
                    *guard = Some((Instant::now(), parsed.clone()));
                }
                if let Some(path) = disk_cache {
                    // The disk copy only spares the next process a probe; the live listing
                    // stands without it.
                    let _ = probe::write_cached(&path, &bin, &output.stdout);
                }
                return parsed;
            }
        }
    }
    models()
}

/// The exact `--model` ID to send: the mapped wire ID for `level`, else the
/// model's default variant (medium, then high, ...), else `model.id`.
#[must_use]
pub fn wire_model_for_level(model: &Model, level: Option<ModelThinkingLevel>) -> String {
    let Some(map) = &model.thinking_level_map else {
        return model.id.clone();
    };
    let pick = |level: ModelThinkingLevel| map.get(&level).cloned().flatten();
    if let Some(wire) = level.filter(|_| model.reasoning).and_then(pick) {
        return wire;
    }
    [
        ModelThinkingLevel::Medium,
        ModelThinkingLevel::High,
        ModelThinkingLevel::Low,
        ModelThinkingLevel::Xhigh,
        ModelThinkingLevel::Max,
        ModelThinkingLevel::Minimal,
        ModelThinkingLevel::Off,
    ]
    .into_iter()
    .find_map(pick)
    .unwrap_or_else(|| model.id.clone())
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
                        return (delta, Some(cached.chat_id.clone()));
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

fn usage_field(value: &Value, camel: &str, snake: &str) -> Option<u64> {
    value[camel].as_u64().or_else(|| value[snake].as_u64())
}

fn apply_usage(model: &Model, response_model: Option<&str>, usage: &mut Usage, value: &Value) {
    let input = usage_field(value, "inputTokens", "input_tokens").unwrap_or(usage.input);
    let output = usage_field(value, "outputTokens", "output_tokens").unwrap_or(usage.output);
    let cache_read =
        usage_field(value, "cacheReadTokens", "cache_read_tokens").unwrap_or(usage.cache_read);
    let cache_write =
        usage_field(value, "cacheWriteTokens", "cache_write_tokens").unwrap_or(usage.cache_write);
    *usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost::default(),
    };
    apply_estimated_cost(model, response_model, usage, reported_cost_usd(value));
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

/// Open content blocks while reading one stream.
#[derive(Default)]
struct BlockState {
    thinking_index: Option<u64>,
    text_index: Option<u64>,
    /// Every text block's content so far, for spotting a final full repeat.
    all_text: String,
}

impl BlockState {
    fn close_thinking(&mut self, partial: &AssistantMessage, writer: &AssistantMessageEventWriter) {
        if let Some(idx) = self.thinking_index.take() {
            if let Some(AssistantContent::Thinking(content)) = partial.content.get(idx as usize) {
                writer.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: idx,
                    content: content.thinking.clone(),
                    partial: partial.clone(),
                });
            }
        }
    }

    fn close_text(&mut self, partial: &mut AssistantMessage, writer: &AssistantMessageEventWriter) {
        if let Some(idx) = self.text_index.take() {
            let final_text = if let Some(AssistantContent::Text(content)) =
                partial.content.get_mut(idx as usize)
            {
                let trimmed = content.text.trim_end_matches('\n').to_string();
                content.text.clone_from(&trimmed);
                Some(trimmed)
            } else {
                None
            };
            if let Some(content) = final_text {
                writer.push(AssistantMessageEvent::TextEnd {
                    content_index: idx,
                    content,
                    partial: partial.clone(),
                });
            }
        }
    }

    fn push_thinking(
        &mut self,
        chunk: &str,
        partial: &mut AssistantMessage,
        writer: &AssistantMessageEventWriter,
    ) {
        let idx = if let Some(idx) = self.thinking_index {
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
            self.thinking_index = Some(idx);
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

    fn current_text<'a>(&self, partial: &'a AssistantMessage) -> &'a str {
        match self
            .text_index
            .and_then(|idx| partial.content.get(idx as usize))
        {
            Some(AssistantContent::Text(content)) => &content.text,
            _ => "",
        }
    }

    /// With `--stream-partial-output` Cursor emits timestamped deltas and then
    /// repeats the segment's full text untimestamped; a snapshot only
    /// contributes what was not already streamed.
    fn push_text(
        &mut self,
        chunk: &str,
        snapshot: bool,
        partial: &mut AssistantMessage,
        writer: &AssistantMessageEventWriter,
    ) {
        let delta = if snapshot {
            let current = self.current_text(partial);
            if chunk == current || chunk == self.all_text {
                return;
            }
            chunk.strip_prefix(current).unwrap_or(chunk)
        } else {
            chunk
        };
        if delta.is_empty() {
            return;
        }
        let delta = delta.to_string();
        self.close_thinking(partial, writer);
        let idx = if let Some(idx) = self.text_index {
            idx
        } else {
            let idx = partial.content.len() as u64;
            partial.content.push(AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
                rest: Map::default(),
            }));
            writer.push(AssistantMessageEvent::TextStart {
                content_index: idx,
                partial: partial.clone(),
            });
            self.text_index = Some(idx);
            idx
        };
        if let Some(AssistantContent::Text(content)) = partial.content.get_mut(idx as usize) {
            content.text.push_str(&delta);
        }
        self.all_text.push_str(&delta);
        writer.push(AssistantMessageEvent::TextDelta {
            content_index: idx,
            delta,
            partial: partial.clone(),
        });
    }
}

/// `tool_call` payloads are keyed by kind (`{"shellToolCall":{"args":{..}}}`) or are a
/// generic `{"function":{"name":..,"arguments":"<json>"}}`.
fn cursor_tool_line(tool_call: &Value) -> String {
    let Some((kind, body)) = tool_call.as_object().and_then(|map| map.iter().next()) else {
        return tool_progress_line("tool", &Value::Null);
    };
    if kind == "function" {
        let name = body["name"].as_str().unwrap_or("tool");
        let args = match &body["arguments"] {
            Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
            other => other.clone(),
        };
        return tool_progress_line(name, &args);
    }
    let name = kind.strip_suffix("ToolCall").unwrap_or(kind);
    tool_progress_line(name, &body["args"])
}

fn record_chat_id(event: &Value, chat_id: &mut Option<String>, partial: &mut AssistantMessage) {
    if let Some(sid) = event["session_id"].as_str().filter(|s| !s.is_empty()) {
        *chat_id = Some(sid.to_string());
        partial.response_id = Some(sid.to_string());
    }
}

async fn run_stream(
    writer: AssistantMessageEventWriter,
    model: Model,
    context: Context,
    base: StreamOptions,
    reasoning: Option<ModelThinkingLevel>,
) {
    run_stream_with_bin(writer, model, context, base, reasoning, cursor_bin()).await;
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
            "Cursor CLI (cursor-agent) is not installed.".to_string(),
        );
        return;
    };

    let cwd = extract_cwd(&context);
    let session_id = base.session_id.clone();
    let (prompt, resume_id) = build_prompt_and_resume(&context, &model.id, session_id.as_deref());
    let wire_model = wire_model_for_level(&model, reasoning);

    let mut cmd = Command::new(&bin);
    cmd.args([
        "--print",
        "--output-format",
        "stream-json",
        "--stream-partial-output",
        "--force",
        "--trust",
    ]);
    cmd.arg("--workspace").arg(&cwd);
    cmd.args(["--model", &wire_model]);
    if let Some(chat_id) = &resume_id {
        cmd.args(["--resume", chat_id]);
    }
    // `--` keeps prompts that start with `-` from parsing as options.
    cmd.arg("--")
        .arg(&prompt)
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
                format!("Could not start Cursor CLI ({}): {error}", bin.display()),
            );
            return;
        }
    };

    let Some(stdout) = child.stdout.take() else {
        push_error(
            &writer,
            partial,
            ErrorStopReason::Error,
            "Cursor CLI stdout unavailable".to_string(),
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
    let mut blocks = BlockState::default();
    let mut chat_id: Option<String> = resume_id;
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
                    format!("Failed reading Cursor CLI output: {error}"),
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
                record_chat_id(&event, &mut chat_id, &mut partial);
                if let Some(rm) = event["model"].as_str().filter(|s| !s.is_empty()) {
                    partial.response_model = Some(rm.to_string());
                }
            }
            Some("thinking") => {
                record_chat_id(&event, &mut chat_id, &mut partial);
                match event["subtype"].as_str() {
                    Some("delta") => {
                        if let Some(chunk) = event["text"].as_str().filter(|s| !s.is_empty()) {
                            blocks.push_thinking(chunk, &mut partial, &writer);
                        }
                    }
                    Some("completed") => blocks.close_thinking(&partial, &writer),
                    _ => {}
                }
            }
            Some("assistant") => {
                record_chat_id(&event, &mut chat_id, &mut partial);
                let msg = &event["message"];
                let chunk = match &msg["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter(|part| part["type"].as_str().is_none_or(|t| t == "text"))
                        .filter_map(|part| part["text"].as_str())
                        .collect(),
                    _ => String::new(),
                };
                if !chunk.is_empty() {
                    let snapshot = event["timestamp_ms"].is_null();
                    blocks.push_text(&chunk, snapshot, &mut partial, &writer);
                }
            }
            Some("tool_call") => {
                record_chat_id(&event, &mut chat_id, &mut partial);
                blocks.close_text(&mut partial, &writer);
                if event["subtype"].as_str() == Some("started") {
                    let line = cursor_tool_line(&event["tool_call"]);
                    blocks.push_thinking(&line, &mut partial, &writer);
                } else {
                    blocks.close_thinking(&partial, &writer);
                }
            }
            Some("result") => {
                record_chat_id(&event, &mut chat_id, &mut partial);
                if event["usage"].is_object() {
                    apply_usage(
                        &model,
                        partial.response_model.as_deref(),
                        &mut partial.usage,
                        &event["usage"],
                    );
                }
                if let Some(reported) = reported_cost_usd(&event) {
                    apply_estimated_cost(
                        &model,
                        partial.response_model.as_deref(),
                        &mut partial.usage,
                        Some(reported),
                    );
                }
                if event["is_error"].as_bool() == Some(true) {
                    result_error = Some(
                        event["result"]
                            .as_str()
                            .or_else(|| event["error"].as_str())
                            .filter(|s| !s.is_empty())
                            .unwrap_or("Cursor CLI returned an error")
                            .to_string(),
                    );
                } else if let Some(body) = event["result"].as_str().filter(|s| !s.is_empty()) {
                    result_text = Some(body.trim_end_matches('\n').to_string());
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
                    || format!("Cursor CLI exited with {exit}"),
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
                format!("Failed waiting for Cursor CLI: {error}"),
            );
            return;
        }
    }

    blocks.close_thinking(&partial, &writer);
    if blocks.all_text.is_empty() {
        if let Some(body) = result_text {
            blocks.push_text(&body, true, &mut partial, &writer);
        }
    }
    blocks.close_text(&mut partial, &writer);

    if let (Some(sid), Some(cid)) = (session_id, chat_id) {
        if let Ok(mut guard) = sessions().lock() {
            guard.insert(
                sid,
                CachedChat {
                    chat_id: cid,
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

/// Streaming provider implementation backed by the local Cursor CLI (`cursor-agent`).
pub struct CursorCliProvider;

impl Provider for CursorCliProvider {
    fn api(&self) -> &str {
        CURSOR_CLI_API
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
            Message::User(radiata_types::ai::UserMessage {
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
        let sid = "test-session-cursor_cli-absorb-resume";
        sessions().lock().unwrap().insert(
            sid.to_string(),
            CachedChat {
                chat_id: "native-conv-1".into(),
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
    use crate::models::get_supported_thinking_levels;

    #[test]
    fn parses_cursor_list_models_with_thinking_maps() {
        let raw = "\x1b[2K\x1b[1mAvailable models\x1b[0m\n\n\
auto - Auto (default)\n\
composer-2.5 - Composer 2.5\n\
claude-opus-5-5-low - Claude Opus 5.5 1M Low\n\
claude-opus-5-5-low-fast - Claude Opus 5.5 1M Low Fast\n\
claude-opus-5-5-medium - Claude Opus 5.5 1M\n\
claude-opus-5-5-high - Claude Opus 5.5 1M High\n\
claude-opus-5-5-high-fast - Claude Opus 5.5 1M High Fast\n\
claude-opus-5-5-max - Claude Opus 5.5 1M Max\n\
grok-4.7-low - Grok 4.7  Low\n\
grok-4.7-low-fast - Grok 4.7  Low Fast\u{200b}\u{200b}\n\
grok-4.7-xhigh - Grok 4.7  Extra High\n\
gpt-5.3-codex-low - Codex 5.3 Low\n\
gpt-5.3-codex - Codex 5.3\n\
gpt-5.3-codex-high - Codex 5.3 High\n\
gpt-5.5-none - GPT-5.5 1M None\n\
gpt-5.5-extra-high - GPT-5.5 1M Extra High\n\
claude-4.6-opus-high-thinking - Claude Opus 4.6 1M Thinking\n\
claude-4.6-opus-max-thinking - Claude Opus 4.6 1M Max Thinking\n";
        let parsed = parse_models_output(raw);
        let find = |id: &str| {
            parsed
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("{id} missing from {:?}", parsed))
        };
        let ids: Vec<&str> = parsed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "auto",
                "composer-2.5",
                "claude-opus-5-5",
                "claude-opus-5-5-fast",
                "grok-4.7",
                "grok-4.7-fast",
                "gpt-5.3-codex",
                "gpt-5.5",
                "claude-4.6-opus-thinking",
            ]
        );

        let auto = find("auto");
        assert_eq!(auto.name, "Auto");
        assert!(!auto.reasoning);
        assert_eq!(
            wire_model_for_level(auto, Some(ModelThinkingLevel::High)),
            "auto"
        );

        let opus = find("claude-opus-5-5");
        assert_eq!(opus.provider, CURSOR_PROVIDER);
        assert_eq!(opus.api, CURSOR_CLI_API);
        assert_eq!(opus.name, "Claude Opus 5.5 1M");
        assert_eq!(opus.context_window, 1_000_000);
        assert_eq!(
            get_supported_thinking_levels(opus),
            vec![
                ModelThinkingLevel::Low,
                ModelThinkingLevel::Medium,
                ModelThinkingLevel::High,
                ModelThinkingLevel::Max,
            ]
        );
        assert_eq!(
            wire_model_for_level(opus, Some(ModelThinkingLevel::Low)),
            "claude-opus-5-5-low"
        );
        assert_eq!(
            wire_model_for_level(opus, Some(ModelThinkingLevel::High)),
            "claude-opus-5-5-high"
        );
        assert_eq!(
            wire_model_for_level(opus, Some(ModelThinkingLevel::Off)),
            "claude-opus-5-5-medium"
        );
        assert_eq!(wire_model_for_level(opus, None), "claude-opus-5-5-medium");

        let opus_fast = find("claude-opus-5-5-fast");
        assert_eq!(opus_fast.name, "Claude Opus 5.5 1M Fast");
        assert_eq!(
            wire_model_for_level(opus_fast, Some(ModelThinkingLevel::High)),
            "claude-opus-5-5-high-fast"
        );

        let grok_fast = find("grok-4.7-fast");
        assert_eq!(grok_fast.name, "Grok 4.7 Fast");
        assert_eq!(
            wire_model_for_level(grok_fast, Some(ModelThinkingLevel::Low)),
            "grok-4.7-low-fast"
        );
        assert_eq!(
            wire_model_for_level(find("grok-4.7"), Some(ModelThinkingLevel::Xhigh)),
            "grok-4.7-xhigh"
        );

        let codex = find("gpt-5.3-codex");
        assert_eq!(
            wire_model_for_level(codex, Some(ModelThinkingLevel::Medium)),
            "gpt-5.3-codex"
        );

        let gpt55 = find("gpt-5.5");
        assert_eq!(
            wire_model_for_level(gpt55, Some(ModelThinkingLevel::Off)),
            "gpt-5.5-none"
        );
        assert_eq!(
            wire_model_for_level(gpt55, Some(ModelThinkingLevel::Xhigh)),
            "gpt-5.5-extra-high"
        );

        let opus46 = find("claude-4.6-opus-thinking");
        assert_eq!(opus46.name, "Claude Opus 4.6 1M Thinking");
        assert_eq!(
            wire_model_for_level(opus46, Some(ModelThinkingLevel::Max)),
            "claude-4.6-opus-max-thinking"
        );
    }

    #[test]
    fn builtin_registry_registers_cursor_cli_provider() {
        crate::registry::ensure_builtins();
        let provider = crate::registry::get_api_provider(CURSOR_CLI_API);
        assert!(provider.is_some());
        assert_eq!(provider.unwrap().api(), CURSOR_CLI_API);

        let catalog = builtin_models();
        for id in [
            "auto",
            "claude-opus-5-5",
            "claude-opus-5-5-fast",
            "composer-2.5",
            "grok-4.7",
            "gemini-4-neon",
        ] {
            assert!(
                catalog.iter().any(|m| m.id == id),
                "{id} in builtin catalog"
            );
        }
        let fast = catalog
            .iter()
            .find(|m| m.id == "claude-opus-5-5-fast")
            .unwrap();
        assert_eq!(
            wire_model_for_level(fast, Some(ModelThinkingLevel::Xhigh)),
            "claude-opus-5-5-xhigh-fast"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streams_deltas_and_resume_from_fake_cursor_agent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "pa-cursor-cli-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("fake-cursor-agent");
        let args_log = dir.join("args.txt");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"{}\"\nprintf '%s\\n' '---' >> \"{}\"\n\
             printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"chat-42\",\"model\":\"Claude Opus 5.5 1M High\"}}'\n\
             printf '%s\\n' '{{\"type\":\"thinking\",\"subtype\":\"delta\",\"text\":\"Plan: \",\"session_id\":\"chat-42\"}}'\n\
             printf '%s\\n' '{{\"type\":\"thinking\",\"subtype\":\"completed\",\"session_id\":\"chat-42\"}}'\n\
             printf '%s\\n' '{{\"type\":\"tool_call\",\"subtype\":\"started\",\"call_id\":\"c1\",\"tool_call\":{{\"shellToolCall\":{{\"args\":{{\"command\":\"cargo test -p radiata-ai\"}}}}}},\"session_id\":\"chat-42\"}}'\n\
             printf '%s\\n' '{{\"type\":\"tool_call\",\"subtype\":\"completed\",\"call_id\":\"c1\",\"tool_call\":{{\"shellToolCall\":{{\"args\":{{\"command\":\"cargo test -p radiata-ai\"}},\"result\":{{}}}}}},\"session_id\":\"chat-42\"}}'\n\
             printf '%s\\n' '{{\"type\":\"tool_call\",\"subtype\":\"started\",\"call_id\":\"c2\",\"tool_call\":{{\"readToolCall\":{{\"args\":{{\"path\":\"/tmp/a.rs\"}}}}}},\"session_id\":\"chat-42\"}}'\n\
             printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Hello \"}}]}},\"session_id\":\"chat-42\",\"timestamp_ms\":1}}'\n\
             printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Cursor!\"}}]}},\"session_id\":\"chat-42\",\"timestamp_ms\":2}}'\n\
             printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Hello Cursor!\"}}]}},\"session_id\":\"chat-42\"}}'\n\
             printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Hello Cursor!\",\"session_id\":\"chat-42\",\"usage\":{{\"inputTokens\":30,\"outputTokens\":6,\"cacheReadTokens\":4,\"cacheWriteTokens\":2}}}}'\n",
            args_log.display(),
            args_log.display()
        );
        std::fs::write(&script_path, script).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let opus = builtin_models()
            .into_iter()
            .find(|m| m.id == "claude-opus-5-5")
            .unwrap();
        let user = |text: &str| {
            Message::User(radiata_types::ai::UserMessage {
                content: UserMessageContent::Text(text.to_string()),
                timestamp: now_ms(),
                rest: Map::default(),
            })
        };
        let session = "test-session-cursor".to_string();

        let run = |context: Context| {
            let (writer, stream) = create_assistant_message_event_stream();
            let handle = tokio::spawn(run_stream_with_bin(
                writer,
                opus.clone(),
                context,
                StreamOptions {
                    session_id: Some(session.clone()),
                    ..Default::default()
                },
                Some(ModelThinkingLevel::High),
                Some(script_path.clone()),
            ));
            (stream, handle)
        };

        let first_context = Context {
            system_prompt: None,
            messages: vec![user("-say hi")],
            tools: None,
        };
        let (mut stream, handle) = run(first_context.clone());
        let mut thinking = Vec::new();
        let mut deltas = Vec::new();
        while let Some(event) = stream.next_event().await {
            match event {
                AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking.push(delta),
                AssistantMessageEvent::TextDelta { delta, .. } => deltas.push(delta),
                _ => {}
            }
        }
        let message = stream.result().await;
        handle.await.unwrap();

        assert_eq!(
            thinking,
            vec![
                "Plan: ",
                "\n▸ shell cargo test -p radiata-ai\n",
                "\n▸ read /tmp/a.rs\n"
            ]
        );
        assert_eq!(deltas, vec!["Hello ", "Cursor!"]);
        let texts: Vec<&str> = message
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello Cursor!"]);
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.response_id.as_deref(), Some("chat-42"));
        assert_eq!(
            message.response_model.as_deref(),
            Some("Claude Opus 5.5 1M High")
        );
        assert_eq!(message.usage.input, 30);
        assert_eq!(message.usage.output, 6);
        assert_eq!(message.usage.cache_read, 4);
        assert_eq!(message.usage.cache_write, 2);
        assert_eq!(message.usage.total_tokens, 42);
        // Opus list price: $5 in, $25 out, $0.50 read, $6.25 write per million tokens.
        assert!((message.usage.cost.total.as_f64() - 314.5e-6).abs() < 1e-12);

        let mut second_context = first_context;
        second_context
            .messages
            .push(Message::Assistant(message.clone()));
        second_context.messages.push(user("again"));
        let (mut stream, handle) = run(second_context);
        while stream.next_event().await.is_some() {}
        let _ = stream.result().await;
        handle.await.unwrap();

        let recorded = std::fs::read_to_string(&args_log).unwrap();
        let invocations: Vec<Vec<&str>> = recorded
            .split("---\n")
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| chunk.lines().collect())
            .collect();
        assert_eq!(invocations.len(), 2);
        let first = &invocations[0];
        assert!(first
            .windows(2)
            .any(|w| w == ["--model", "claude-opus-5-5-high"]));
        assert!(first.contains(&"--stream-partial-output"));
        assert!(!first.contains(&"--resume"));
        assert_eq!(first[first.len() - 2..], ["--", "-say hi"]);
        let second = &invocations[1];
        assert!(second.windows(2).any(|w| w == ["--resume", "chat-42"]));
        assert_eq!(second.last(), Some(&"again"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_models_carry_list_prices_and_usage_is_priced() {
        let catalog = builtin_models();
        let model = |id: &str| catalog.iter().find(|m| m.id == id).expect(id).clone();
        let composer = model("composer-2.5");
        assert_eq!(composer.cost.input.as_f64(), 1.25);
        assert_eq!(composer.cost.output.as_f64(), 6.0);
        assert_eq!(model("gemini-4-neon").cost.output.as_f64(), 15.0);
        assert_eq!(model("auto").cost, zero_model_cost());

        let mut usage = Usage::default();
        apply_usage(
            &composer,
            None,
            &mut usage,
            &serde_json::json!({"inputTokens": 1_000_000, "outputTokens": 0}),
        );
        assert!((usage.cost.total.as_f64() - 1.25).abs() < 1e-12);

        // `auto` has no list price of its own; the routed response model prices it.
        let auto = model("auto");
        apply_usage(
            &auto,
            Some("composer-2.5"),
            &mut usage,
            &serde_json::json!({"inputTokens": 1_000_000, "outputTokens": 0}),
        );
        assert!((usage.cost.total.as_f64() - 1.25).abs() < 1e-12);

        apply_usage(
            &composer,
            None,
            &mut usage,
            &serde_json::json!({"inputTokens": 10, "outputTokens": 1, "total_cost_usd": 0.5}),
        );
        assert_eq!(usage.cost.total.as_f64(), 0.5);

        let mut empty = Usage::default();
        apply_usage(
            &composer,
            None,
            &mut empty,
            &serde_json::json!({"inputTokens": 0, "outputTokens": 0}),
        );
        assert_eq!(empty.cost.total.as_f64(), 0.0);
    }

    #[test]
    fn parses_cursor_cli_auth_status() {
        let status = parse_auth_status("✓ Logged in as dev@example.com\nTeam: Acme Corp (id 12345)\n")
            .expect("logged in status");
        assert!(status.logged_in);
        assert_eq!(status.account.as_deref(), Some("dev@example.com"));
        assert_eq!(status.team.as_deref(), Some("Acme Corp"));

        let out = parse_auth_status("Not logged in. Run `cursor-agent login`.\n")
            .expect("logged out status");
        assert!(!out.logged_in);
        assert_eq!(out.account, None);
    }
}
