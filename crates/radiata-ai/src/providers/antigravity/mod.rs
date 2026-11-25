//! Antigravity CLI (`agy`) streaming provider for Prime Agent.
//!
//! Surfaces Antigravity models (Gemini 4 Neon/Argon/Hydrogen, Gemini 3.8 Flash,
//! Claude Opus/Sonnet 5.5 via Antigravity, etc.) directly in `ModelRegistry` and
//! streams completions through the local Antigravity CLI (`agy` or T3 Code's
//! managed `antigravity` binary).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

use crate::cli_pricing::{apply_estimated_cost, list_price_for_model, reported_cost_usd};
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStream, AssistantMessageEventWriter,
};
use crate::providers::cli_model_probe as probe;
use crate::registry::Provider;
use crate::types::{
    zero_model_cost, AssistantContent, AssistantMessage, AssistantMessageEvent, Context,
    DoneStopReason, ErrorStopReason, Message, Model, ModelInput, ModelThinkingLevel,
    SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingContent, ThinkingLevelMap,
    ToolResultMessage, Usage, UsageCost, UserMessageContent,
};
use crate::utils_inner::diagnostics::now_ms;

const ACP_INIT_TIMEOUT: Duration = Duration::from_secs(20);
const ACP_POOL_TTL: Duration = Duration::from_secs(600);
const ACP_POOL_MAX: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AcpPoolKey {
    bin: PathBuf,
    cwd: PathBuf,
    acp_session: String,
}

struct WarmAcp {
    child: tokio::process::Child,
    io: AcpIo,
    model_id: String,
    effort: Option<&'static str>,
    last_used: Instant,
}

fn acp_pool() -> &'static tokio::sync::Mutex<HashMap<AcpPoolKey, WarmAcp>> {
    static POOL: OnceLock<tokio::sync::Mutex<HashMap<AcpPoolKey, WarmAcp>>> = OnceLock::new();
    POOL.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

async fn take_warm_acp(bin: &Path, cwd: &Path, acp_session: &str) -> Option<WarmAcp> {
    let key = AcpPoolKey {
        bin: bin.to_path_buf(),
        cwd: cwd.to_path_buf(),
        acp_session: acp_session.to_string(),
    };
    let mut pool = acp_pool().lock().await;
    pool.retain(|_, worker| worker.last_used.elapsed() <= ACP_POOL_TTL);
    let mut worker = pool.remove(&key)?;
    if worker.child.try_wait().ok().flatten().is_some() {
        return None;
    }
    Some(worker)
}

async fn put_warm_acp(key: AcpPoolKey, mut worker: WarmAcp) {
    if worker.child.try_wait().ok().flatten().is_some() {
        return;
    }
    worker.last_used = Instant::now();
    let mut pool = acp_pool().lock().await;
    pool.retain(|_, existing| existing.last_used.elapsed() <= ACP_POOL_TTL);
    while pool.len() >= ACP_POOL_MAX {
        let Some(oldest) = pool
            .iter()
            .min_by_key(|(_, existing)| existing.last_used)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        pool.remove(&oldest);
    }
    pool.insert(key, worker);
}

#[cfg(test)]
async fn clear_warm_acp_pool() {
    acp_pool().lock().await.clear();
}

const TOOL_ARG_KEYS: &[&str] = &[
    "CommandLine",
    "command",
    "cmd",
    "AbsolutePath",
    "TargetFile",
    "Path",
    "path",
    "file_path",
    "filePath",
    "target_file",
    "DirectoryPath",
    "directory",
    "Query",
    "query",
    "Pattern",
    "pattern",
    "glob_pattern",
    "globPattern",
    "SearchPath",
    "Url",
    "url",
    "prompt",
    "description",
];

/// One-line `▸ <tool> <arg>` progress marker streamed into the thinking block
/// while a CLI agent runs a tool.
pub(crate) fn tool_progress_line(name: &str, args: &Value) -> String {
    let arg = match args {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => {
            if name == "schedule" {
                if let Some(secs) = map.get("DurationSeconds").and_then(Value::as_u64) {
                    let dur = if secs >= 60 && secs % 60 == 0 {
                        format!("{}m", secs / 60)
                    } else if secs >= 60 {
                        format!("{}m {}s", secs / 60, secs % 60)
                    } else {
                        format!("{secs}s")
                    };
                    let prompt = map
                        .get("Prompt")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    Some(match prompt {
                        Some(p) => format!("{dur} · {p}"),
                        None => dur,
                    })
                } else {
                    map.get("CronExpression")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                }
            } else {
                None
            }
            .or_else(|| {
                TOOL_ARG_KEYS.iter().find_map(|key| match map.get(*key) {
                    Some(Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
                    Some(Value::Array(items)) if !items.is_empty() => Some(
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    _ => None,
                })
            })
        }
        _ => None,
    };
    let arg = arg
        .as_deref()
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .map(|line| {
            if line.chars().count() > 160 {
                format!("{}…", line.chars().take(160).collect::<String>())
            } else {
                line.to_string()
            }
        });
    match arg {
        Some(arg) => format!("\n▸ {name} {arg}\n"),
        None => format!("\n▸ {name}\n"),
    }
}

/// Open thinking/text blocks while reading one Antigravity stream.
#[derive(Default)]
struct BlockState {
    thinking_index: Option<u64>,
    text_index: Option<u64>,
    any_text: bool,
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
            if let Some(AssistantContent::Text(content)) = partial.content.get_mut(idx as usize) {
                let trimmed = content.text.trim_end_matches('\n').to_string();
                content.text.clone_from(&trimmed);
                writer.push(AssistantMessageEvent::TextEnd {
                    content_index: idx,
                    content: trimmed,
                    partial: partial.clone(),
                });
            }
        }
    }

    fn close_all(&mut self, partial: &mut AssistantMessage, writer: &AssistantMessageEventWriter) {
        self.close_thinking(partial, writer);
        self.close_text(partial, writer);
    }

    fn push_thinking(
        &mut self,
        chunk: &str,
        partial: &mut AssistantMessage,
        writer: &AssistantMessageEventWriter,
    ) {
        if chunk.is_empty() {
            return;
        }
        self.close_text(partial, writer);
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

    fn push_text(
        &mut self,
        chunk: &str,
        partial: &mut AssistantMessage,
        writer: &AssistantMessageEventWriter,
    ) {
        if chunk.is_empty() {
            return;
        }
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
            content.text.push_str(chunk);
        }
        self.any_text = true;
        writer.push(AssistantMessageEvent::TextDelta {
            content_index: idx,
            delta: chunk.to_string(),
            partial: partial.clone(),
        });
    }
}

/// Wire API identifier for Antigravity CLI models in `ModelRegistry`.
pub const ANTIGRAVITY_API: &str = "antigravity-cli";
/// Provider identifier for Antigravity models in `ModelRegistry`.
pub const ANTIGRAVITY_PROVIDER: &str = "antigravity";

const MODEL_CACHE_TTL: Duration = Duration::from_secs(300);
/// The cross-process copy of the last `agy models` listing, under `<agent dir>/cache/`.
const MODEL_DISK_CACHE: &str = "antigravity-models.json";

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
        cost: list_price_for_model(id).unwrap_or_else(zero_model_cost),
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

/// Sign-in state reported by `<antigravity_bin> models`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravityCliAuthStatus {
    pub logged_in: bool,
    pub model_count: usize,
    pub detail: Option<String>,
}

/// Interpret `<antigravity_bin> models` output into an account status.
#[must_use]
pub fn parse_auth_models_status(
    stdout: &str,
    stderr: &str,
    success: bool,
) -> AntigravityCliAuthStatus {
    let parsed = parse_models_output(stdout);
    if success && !parsed.is_empty() {
        return AntigravityCliAuthStatus {
            logged_in: true,
            model_count: parsed.len(),
            detail: None,
        };
    }
    let detail = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.eq_ignore_ascii_case("fetching available models..."))
        .map(str::to_string);
    AntigravityCliAuthStatus {
        logged_in: false,
        model_count: 0,
        detail,
    }
}

/// Check whether the installed `agy` / `antigravity` CLI has an active Google session.
///
/// Reuses the warm process model cache unless `force` is true so `/login` stays fast.
/// `None` when no CLI is installed or the check did not finish, so the state is unknown.
#[must_use]
pub fn auth_status(force: bool) -> Option<AntigravityCliAuthStatus> {
    let bin = antigravity_bin()?;
    if !force {
        if let Ok(guard) = model_cache().lock() {
            if let Some((fetched_at, cached)) = guard.as_ref() {
                if fetched_at.elapsed() < MODEL_CACHE_TTL && !cached.is_empty() {
                    return Some(AntigravityCliAuthStatus {
                        logged_in: true,
                        model_count: cached.len(),
                        detail: None,
                    });
                }
            }
        }
    }
    // A timeout leaves the session state unknown, not signed out.
    let timeout = if force {
        probe::FORCE_PROBE_TIMEOUT
    } else {
        probe::PROBE_TIMEOUT
    };
    let output = probe::run_bounded(&bin, &["models"], timeout)?;
    let status = parse_auth_models_status(&output.stdout, &output.stderr, output.success);
    if status.logged_in {
        let parsed = parse_models_output(&output.stdout);
        if !parsed.is_empty() {
            if let Ok(mut guard) = model_cache().lock() {
                *guard = Some((Instant::now(), parsed));
            }
            if let Some(path) = probe::cache_path(MODEL_DISK_CACHE) {
                let _ = probe::write_cached(&path, &bin, &output.stdout);
            }
        }
    }
    Some(status)
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
    if let Some(output) = probe::run_bounded(&bin, &["models"], timeout) {
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
                        return (delta, Some(cached.conversation_id.clone()));
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

    if acp_enabled() {
        let request = AcpRequest {
            bin: &bin,
            cwd: &cwd,
            model: &model,
            context: &context,
            signal: base.signal.as_ref(),
            session_id: session_id.clone(),
            prompt: &prompt,
            resume_id: resume_id.clone(),
            effort,
        };
        if run_acp(&writer, request).await {
            return;
        }
    }

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
    let mut blocks = BlockState::default();
    let mut seen_tool_steps: HashSet<String> = HashSet::new();
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
                for key in ["thinking_delta", "thought", "thinking"] {
                    if let Some(chunk) = update[key].as_str() {
                        blocks.push_thinking(chunk, &mut partial, &writer);
                    }
                }
                match update["step_type"].as_str() {
                    Some("agent_response") => {
                        if let Some(delta) = update["text_delta"].as_str() {
                            blocks.push_text(delta, &mut partial, &writer);
                        }
                        if update["usage"].is_object() {
                            apply_usage(
                                &model,
                                partial.response_model.as_deref(),
                                &mut partial.usage,
                                &update["usage"],
                            );
                        }
                    }
                    Some("tool") => {
                        let step_key = update["step_index"].to_string();
                        if seen_tool_steps.insert(step_key) {
                            let name = update["tool_name"]
                                .as_str()
                                .or_else(|| update["tool_info"]["name"].as_str())
                                .unwrap_or("tool");
                            let line = tool_progress_line(name, &update["tool_info"]["parameters"]);
                            blocks.push_thinking(&line, &mut partial, &writer);
                        }
                    }
                    _ => {}
                }
            }
            Some("result") => {
                let res = &event["result"];
                if let Some(cid) = res["conversation_id"].as_str().filter(|s| !s.is_empty()) {
                    conversation_id = Some(cid.to_string());
                    partial.response_id = Some(cid.to_string());
                }
                if res["usage"].is_object() {
                    apply_usage(
                        &model,
                        partial.response_model.as_deref(),
                        &mut partial.usage,
                        &res["usage"],
                    );
                }
                if let Some(reported) = reported_cost_usd(res) {
                    apply_estimated_cost(
                        &model,
                        partial.response_model.as_deref(),
                        &mut partial.usage,
                        Some(reported),
                    );
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

    blocks.close_all(&mut partial, &writer);
    if !blocks.any_text {
        if let Some(body) = result_text {
            blocks.push_text(&body, &mut partial, &writer);
            blocks.close_text(&mut partial, &writer);
        }
    }

    if let Some(banner) = detect_quota_or_auth_banner(&assistant_text(&partial)) {
        push_error(&writer, partial, ErrorStopReason::Error, banner);
        return;
    }

    finish_stream(
        &writer,
        partial,
        &model,
        &context,
        session_id,
        conversation_id,
    );
}

fn assistant_text(partial: &AssistantMessage) -> String {
    partial
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// Upper bound on a reply that can still be a provider banner; longer replies are
/// real answers that merely mention quota or key wording.
const BANNER_MAX_LEN: usize = 500;

/// Antigravity reports quota and credential failures as plain assistant text with a
/// normal `end_turn`. Returns the banner when the whole reply is such a notice.
fn detect_quota_or_auth_banner(text: &str) -> Option<String> {
    const MARKERS: [&str; 6] = [
        "individual quota reached",
        "quota exceeded",
        "rate limit reached",
        "please upgrade your subscription to increase your limits",
        "invalid api key",
        "fix external api key",
    ];
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > BANNER_MAX_LEN {
        return None;
    }
    let lower = trimmed.to_lowercase();
    MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
        .then(|| trimmed.to_string())
}

fn finish_stream(
    writer: &AssistantMessageEventWriter,
    mut partial: AssistantMessage,
    model: &Model,
    context: &Context,
    session_id: Option<String>,
    conversation_id: Option<String>,
) {
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

    let done_reason = if partial.stop_reason == StopReason::Length {
        DoneStopReason::Length
    } else {
        partial.stop_reason = StopReason::Stop;
        DoneStopReason::Stop
    };
    partial.timestamp = now_ms();
    writer.push(AssistantMessageEvent::Done {
        reason: done_reason,
        message: partial.clone(),
    });
    writer.end(Some(partial));
}

/// `PA_ANTIGRAVITY_ACP=0` forces the one-shot `--output-format stream-json` path.
fn acp_enabled() -> bool {
    std::env::var("PA_ANTIGRAVITY_ACP").map_or(true, |raw| {
        let raw = raw.trim();
        !(raw == "0" || raw.eq_ignore_ascii_case("false") || raw.eq_ignore_ascii_case("off"))
    })
}

struct AcpRequest<'a> {
    bin: &'a Path,
    cwd: &'a Path,
    model: &'a Model,
    context: &'a Context,
    signal: Option<&'a CancellationToken>,
    session_id: Option<String>,
    prompt: &'a str,
    resume_id: Option<String>,
    effort: Option<&'static str>,
}

enum AcpError {
    Aborted,
    Closed(String),
    Rpc(String),
}

impl AcpError {
    fn message(self) -> String {
        match self {
            Self::Aborted => "Request was aborted".to_string(),
            Self::Closed(detail) | Self::Rpc(detail) => detail,
        }
    }
}

async fn wait_cancelled(signal: Option<&CancellationToken>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Newline-delimited JSON-RPC 2.0 over the stdio of `agy acp`.
struct AcpIo {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl AcpIo {
    async fn send(&mut self, message: &Value) -> Result<(), AcpError> {
        let mut raw = message.to_string();
        raw.push('\n');
        let written = async {
            self.stdin.write_all(raw.as_bytes()).await?;
            self.stdin.flush().await
        }
        .await;
        written.map_err(|error| AcpError::Closed(format!("Antigravity ACP write failed: {error}")))
    }

    async fn call(
        &mut self,
        method: &str,
        params: Value,
        signal: Option<&CancellationToken>,
        on_update: &mut (dyn FnMut(&Value) + Send),
    ) -> Result<Value, AcpError> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        loop {
            let next = tokio::select! {
                () = wait_cancelled(signal) => return Err(AcpError::Aborted),
                line = self.lines.next_line() => line,
            };
            let line = match next {
                Ok(Some(line)) => line,
                Ok(None) => {
                    return Err(AcpError::Closed(
                        "Antigravity ACP server exited unexpectedly".to_string(),
                    ))
                }
                Err(error) => {
                    return Err(AcpError::Closed(format!(
                        "Failed reading Antigravity ACP output: {error}"
                    )))
                }
            };
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if let Some(method) = message["method"].as_str() {
                if message.get("id").is_some_and(|id| !id.is_null()) {
                    let reply = acp_reply_to_request(method, &message);
                    self.send(&reply).await?;
                } else if method == "session/update" {
                    on_update(&message["params"]["update"]);
                }
                continue;
            }
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error").filter(|e| !e.is_null()) {
                return Err(AcpError::Rpc(
                    error["message"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map_or_else(
                            || format!("Antigravity ACP {method} failed"),
                            str::to_string,
                        ),
                ));
            }
            return Ok(message["result"].clone());
        }
    }
}

/// Auto-approves `session/request_permission`; other agent→client requests are unsupported.
fn acp_reply_to_request(method: &str, message: &Value) -> Value {
    let id = message["id"].clone();
    if method != "session/request_permission" {
        return serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("Method not supported: {method}")},
        });
    }
    let options = message["params"]["options"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let chosen = ["allow_always", "allow_once"]
        .iter()
        .find_map(|kind| {
            options
                .iter()
                .find(|option| option["kind"].as_str() == Some(kind))
        })
        .or_else(|| options.first())
        .and_then(|option| option["optionId"].as_str());
    let outcome = match chosen {
        Some(option_id) => serde_json::json!({"outcome": "selected", "optionId": option_id}),
        None => serde_json::json!({"outcome": "cancelled"}),
    };
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}})
}

fn acp_text(update: &Value) -> Option<&str> {
    update["content"]["text"]
        .as_str()
        .or_else(|| update["content"].as_str())
}

fn acp_tool_line(update: &Value) -> String {
    let title = update["title"].as_str().filter(|s| !s.is_empty());
    let name = update["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or(title)
        .or_else(|| update["kind"].as_str())
        .unwrap_or("tool");
    let args = if update["args"].is_object() {
        &update["args"]
    } else {
        &update["rawInput"]
    };
    let line = tool_progress_line(name, args);
    match title {
        Some(title) if title != name && line == format!("\n▸ {name}\n") => {
            tool_progress_line(name, &Value::String(title.to_string()))
        }
        _ => line,
    }
}

fn apply_acp_update(
    update: &Value,
    model: &Model,
    partial: &mut AssistantMessage,
    blocks: &mut BlockState,
    seen_tools: &mut HashSet<String>,
    writer: &AssistantMessageEventWriter,
) {
    if update["usage"].is_object() {
        apply_acp_usage(
            model,
            partial.response_model.as_deref(),
            &mut partial.usage,
            &update["usage"],
        );
    }
    match update["sessionUpdate"].as_str() {
        Some("agent_thought_chunk") => {
            if let Some(chunk) = acp_text(update) {
                blocks.push_thinking(chunk, partial, writer);
            }
        }
        Some("agent_message_chunk") => {
            if let Some(chunk) = acp_text(update) {
                blocks.push_text(chunk, partial, writer);
            }
        }
        Some(kind @ ("tool_call" | "tool_call_update")) => {
            let call_id = update["toolCallId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if seen_tools.insert(call_id) {
                blocks.push_thinking(&acp_tool_line(update), partial, writer);
            } else if kind == "tool_call_update" && update["status"].as_str() == Some("failed") {
                let line = acp_tool_line(update);
                let label = line.trim().trim_start_matches("▸ ");
                blocks.push_thinking(&format!("\n✗ {label} failed\n"), partial, writer);
            }
        }
        _ => {}
    }
}

fn apply_acp_usage(model: &Model, response_model: Option<&str>, usage: &mut Usage, value: &Value) {
    let input = value["inputTokens"].as_u64().unwrap_or(0);
    let output = value["outputTokens"].as_u64().unwrap_or(0);
    let cache_read = value["cachedReadTokens"].as_u64().unwrap_or(0);
    let cache_write = value["cachedWriteTokens"].as_u64().unwrap_or(0);
    *usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: value["totalTokens"]
            .as_u64()
            .unwrap_or(input + output + cache_read + cache_write),
        cost: UsageCost::default(),
    };
    apply_estimated_cost(model, response_model, usage, reported_cost_usd(value));
}

/// Runs one turn through `agy acp`, reusing a warm persistent ACP process for the same
/// `(bin, cwd, session)` when available so multi-turn chats skip the ~1.4s spawn + load tax.
/// Returns `false` (having pushed no events) when a fresh ACP server never answers `initialize`,
/// so the caller can fall back to `stream-json`.
async fn run_acp(writer: &AssistantMessageEventWriter, request: AcpRequest<'_>) -> bool {
    if let Some(resume) = request.resume_id.as_deref() {
        if let Some(mut warm) = take_warm_acp(request.bin, request.cwd, resume).await {
            let mut partial = empty_assistant_message(request.model);
            partial.response_id = Some(resume.to_string());
            let mut blocks = BlockState::default();
            let mut started = false;
            let turn = acp_prompt_warm(
                &mut warm,
                resume,
                &request,
                &mut partial,
                &mut blocks,
                &mut started,
                writer,
            )
            .await;
            match turn {
                Ok(acp_session) => {
                    blocks.close_all(&mut partial, writer);
                    if let Some(banner) = detect_quota_or_auth_banner(&assistant_text(&partial)) {
                        let _ = warm.child.kill().await;
                        push_error(writer, partial, ErrorStopReason::Error, banner);
                        return true;
                    }
                    put_warm_acp(
                        AcpPoolKey {
                            bin: request.bin.to_path_buf(),
                            cwd: request.cwd.to_path_buf(),
                            acp_session: acp_session.clone(),
                        },
                        warm,
                    )
                    .await;
                    finish_stream(
                        writer,
                        partial,
                        request.model,
                        request.context,
                        request.session_id,
                        Some(acp_session),
                    );
                    return true;
                }
                Err(AcpError::Closed(_)) if !started => {
                    // Pooled process died while idle before streaming anything: retry cold below.
                    let _ = warm.child.kill().await;
                }
                Err(error) => {
                    let _ = warm.child.kill().await;
                    let reason = if matches!(error, AcpError::Aborted) {
                        ErrorStopReason::Aborted
                    } else {
                        ErrorStopReason::Error
                    };
                    push_error(writer, partial, reason, error.message());
                    return true;
                }
            }
        }
    }

    let mut cmd = Command::new(request.bin);
    cmd.args(["acp", "--dangerously-skip-permissions"])
        .current_dir(request.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill().await;
        return false;
    };
    let mut io = AcpIo {
        stdin,
        lines: BufReader::new(stdout).lines(),
        next_id: 0,
    };
    let mut partial = empty_assistant_message(request.model);

    let init = tokio::time::timeout(
        ACP_INIT_TIMEOUT,
        io.call(
            "initialize",
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
            }),
            request.signal,
            &mut |_| {},
        ),
    )
    .await;
    match init {
        Ok(Ok(_)) => {}
        Ok(Err(AcpError::Aborted)) => {
            let _ = child.kill().await;
            push_error(
                writer,
                partial,
                ErrorStopReason::Aborted,
                AcpError::Aborted.message(),
            );
            return true;
        }
        Ok(Err(_)) | Err(_) => {
            let _ = child.kill().await;
            return false;
        }
    }

    writer.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });
    let mut blocks = BlockState::default();
    let turn = acp_turn(&mut io, &request, &mut partial, &mut blocks, writer).await;

    match turn {
        Ok(acp_session) => {
            blocks.close_all(&mut partial, writer);
            if let Some(banner) = detect_quota_or_auth_banner(&assistant_text(&partial)) {
                drop(io);
                let _ = child.kill().await;
                push_error(writer, partial, ErrorStopReason::Error, banner);
                return true;
            }
            put_warm_acp(
                AcpPoolKey {
                    bin: request.bin.to_path_buf(),
                    cwd: request.cwd.to_path_buf(),
                    acp_session: acp_session.clone(),
                },
                WarmAcp {
                    child,
                    io,
                    model_id: request.model.id.clone(),
                    effort: request.effort,
                    last_used: Instant::now(),
                },
            )
            .await;
            finish_stream(
                writer,
                partial,
                request.model,
                request.context,
                request.session_id,
                Some(acp_session),
            );
        }
        Err(error) => {
            drop(io);
            let _ = child.kill().await;
            let reason = if matches!(error, AcpError::Aborted) {
                ErrorStopReason::Aborted
            } else {
                ErrorStopReason::Error
            };
            push_error(writer, partial, reason, error.message());
        }
    }
    true
}

async fn acp_prompt_warm(
    warm: &mut WarmAcp,
    acp_session: &str,
    request: &AcpRequest<'_>,
    partial: &mut AssistantMessage,
    blocks: &mut BlockState,
    started: &mut bool,
    writer: &AssistantMessageEventWriter,
) -> Result<String, AcpError> {
    let signal = request.signal;
    if warm.model_id != request.model.id {
        warm.io
            .call(
                "session/set_config_option",
                serde_json::json!({"sessionId": acp_session, "configId": "model", "value": request.model.id}),
                signal,
                &mut |_| {},
            )
            .await?;
        warm.model_id.clone_from(&request.model.id);
    }
    if warm.effort != request.effort {
        if let Some(effort) = request.effort {
            let set = warm
                .io
                .call(
                    "session/set_config_option",
                    serde_json::json!({"sessionId": acp_session, "configId": "thought_level", "value": effort}),
                    signal,
                    &mut |_| {},
                )
                .await;
            if let Err(AcpError::Aborted) = set {
                return Err(AcpError::Aborted);
            }
        }
        warm.effort = request.effort;
    }

    let mut seen_tools = HashSet::new();
    let result = warm
        .io
        .call(
            "session/prompt",
            serde_json::json!({
                "sessionId": acp_session,
                "prompt": [{"type": "text", "text": request.prompt}],
            }),
            signal,
            &mut |update| {
                if !*started {
                    *started = true;
                    writer.push(AssistantMessageEvent::Start {
                        partial: partial.clone(),
                    });
                }
                apply_acp_update(
                    update,
                    request.model,
                    partial,
                    blocks,
                    &mut seen_tools,
                    writer,
                );
            },
        )
        .await;
    if let Err(AcpError::Aborted) = &result {
        let _ = warm
            .io
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": {"sessionId": acp_session},
            }))
            .await;
    }
    let result = result?;
    if !*started {
        *started = true;
        writer.push(AssistantMessageEvent::Start {
            partial: partial.clone(),
        });
    }
    if result["usage"].is_object() {
        apply_acp_usage(
            request.model,
            partial.response_model.as_deref(),
            &mut partial.usage,
            &result["usage"],
        );
    }
    match result["stopReason"].as_str() {
        Some("cancelled") => Err(AcpError::Aborted),
        Some("refusal") => Err(AcpError::Rpc(
            "Antigravity refused to answer this request".to_string(),
        )),
        Some(raw @ ("max_tokens" | "max_turn_requests")) => {
            partial.stop_reason = StopReason::Length;
            partial.stop_reason_raw = Some(raw.to_string());
            Ok(acp_session.to_string())
        }
        _ => Ok(acp_session.to_string()),
    }
}

async fn acp_turn(
    io: &mut AcpIo,
    request: &AcpRequest<'_>,
    partial: &mut AssistantMessage,
    blocks: &mut BlockState,
    writer: &AssistantMessageEventWriter,
) -> Result<String, AcpError> {
    let signal = request.signal;
    let cwd = request.cwd.display().to_string();
    let mut prompt = request.prompt.to_string();
    let mut acp_session = None;

    if let Some(resume) = &request.resume_id {
        let loaded = io
            .call(
                "session/load",
                serde_json::json!({"sessionId": resume, "cwd": cwd, "mcpServers": []}),
                signal,
                &mut |_| {},
            )
            .await;
        match loaded {
            Ok(_) => acp_session = Some(resume.clone()),
            Err(AcpError::Aborted) => return Err(AcpError::Aborted),
            Err(_) => {
                prompt = build_prompt_and_resume(request.context, &request.model.id, None).0;
            }
        }
    }
    let acp_session = match acp_session {
        Some(session) => session,
        None => {
            let created = io
                .call(
                    "session/new",
                    serde_json::json!({"cwd": cwd, "mcpServers": []}),
                    signal,
                    &mut |_| {},
                )
                .await?;
            created["sessionId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AcpError::Rpc("Antigravity ACP session/new returned no sessionId".to_string())
                })?
                .to_string()
        }
    };
    partial.response_id = Some(acp_session.clone());

    io.call(
        "session/set_config_option",
        serde_json::json!({"sessionId": acp_session, "configId": "model", "value": request.model.id}),
        signal,
        &mut |_| {},
    )
    .await?;
    if let Some(effort) = request.effort {
        let set = io
            .call(
                "session/set_config_option",
                serde_json::json!({"sessionId": acp_session, "configId": "thought_level", "value": effort}),
                signal,
                &mut |_| {},
            )
            .await;
        if let Err(AcpError::Aborted) = set {
            return Err(AcpError::Aborted);
        }
    }

    let mut seen_tools = HashSet::new();
    let result = io
        .call(
            "session/prompt",
            serde_json::json!({
                "sessionId": acp_session,
                "prompt": [{"type": "text", "text": prompt}],
            }),
            signal,
            &mut |update| {
                apply_acp_update(
                    update,
                    request.model,
                    partial,
                    blocks,
                    &mut seen_tools,
                    writer,
                )
            },
        )
        .await;
    if let Err(AcpError::Aborted) = &result {
        let _ = io
            .send(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": {"sessionId": acp_session},
            }))
            .await;
    }
    let result = result?;
    if result["usage"].is_object() {
        apply_acp_usage(
            request.model,
            partial.response_model.as_deref(),
            &mut partial.usage,
            &result["usage"],
        );
    }
    match result["stopReason"].as_str() {
        Some("cancelled") => Err(AcpError::Aborted),
        Some("refusal") => Err(AcpError::Rpc(
            "Antigravity refused to answer this request".to_string(),
        )),
        Some(raw @ ("max_tokens" | "max_turn_requests")) => {
            partial.stop_reason = StopReason::Length;
            partial.stop_reason_raw = Some(raw.to_string());
            Ok(acp_session)
        }
        _ => Ok(acp_session),
    }
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
        let sid = "test-session-antigravity-absorb-resume";
        sessions().lock().unwrap().insert(
            sid.to_string(),
            CachedConversation {
                conversation_id: "native-conv-1".into(),
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
        assert_eq!(neon.cost.input.as_f64(), 2.5);
        assert_eq!(neon.cost.output.as_f64(), 15.0);
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
             printf '%s\\n' '{{\"event\":\"step_update\",\"step_update\":{{\"step_index\":2,\"step_type\":\"tool\",\"state\":\"ACTIVE\",\"tool_name\":\"run_command\",\"tool_info\":{{\"name\":\"run_command\",\"parameters\":{{\"CommandLine\":\"ls /tmp\"}}}}}}}}'\n\
             printf '%s\\n' '{{\"event\":\"step_update\",\"step_update\":{{\"step_index\":2,\"step_type\":\"tool\",\"state\":\"DONE\",\"tool_name\":\"run_command\",\"tool_info\":{{\"name\":\"run_command\",\"parameters\":{{\"CommandLine\":\"ls /tmp\"}},\"output\":\"a\"}}}}}}'\n\
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
            messages: vec![Message::User(radiata_types::ai::UserMessage {
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
        let mut thinking = Vec::new();
        let mut thinking_before_text = false;
        let mut stream_iter = stream;
        while let Some(event) = stream_iter.next_event().await {
            match event {
                AssistantMessageEvent::TextDelta { delta, .. } => deltas.push(delta),
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    thinking_before_text = deltas.is_empty();
                    thinking.push(delta);
                }
                _ => {}
            }
        }
        let message = stream_iter.result().await;
        handle.await.unwrap();

        assert_eq!(thinking, vec!["\n▸ run_command ls /tmp\n"]);
        assert!(thinking_before_text);
        assert_eq!(deltas, vec!["Hello ", "Neon!"]);
        assert!(matches!(
            message.content.first(),
            Some(AssistantContent::Thinking(content)) if content.thinking == "\n▸ run_command ls /tmp\n"
        ));
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
        assert!((message.usage.cost.input.as_f64() - 30e-6).abs() < 1e-12);
        assert!((message.usage.cost.output.as_f64() - 60e-6).abs() < 1e-12);
        assert!((message.usage.cost.total.as_f64() - 90.5e-6).abs() < 1e-12);

        let recorded_args = std::fs::read_to_string(&args_log).unwrap();
        let args: Vec<&str> = recorded_args.lines().collect();
        assert!(args.windows(2).any(|w| w == ["--model", "gemini-4-neon"]));
        assert!(args.windows(2).any(|w| w == ["--effort", "xhigh"]));
        assert!(args.contains(&"-p=Say hi"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn quota_banner_reply_ends_the_stream_with_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "pa-antigravity-quota-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("fake-agy");
        let script = "#!/bin/sh\n\
             printf '%s\\n' '{\"event\":\"init\",\"conversation_id\":\"conv-q\"}'\n\
             printf '%s\\n' '{\"event\":\"result\",\"result\":{\"conversation_id\":\"conv-q\",\"status\":\"SUCCESS\",\"response\":\"\\n⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 4h52m9s.\"}}'\n";
        std::fs::write(&script_path, script).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let model = builtin_models().into_iter().next().unwrap();
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(radiata_types::ai::UserMessage {
                content: UserMessageContent::Text("Say hi".to_string()),
                timestamp: now_ms(),
                rest: Map::default(),
            })],
            tools: None,
        };
        let (writer, mut stream) = create_assistant_message_event_stream();
        let handle = tokio::spawn(run_stream_with_bin(
            writer,
            model,
            context,
            StreamOptions::default(),
            None,
            Some(script_path),
        ));
        let mut saw_error = false;
        let mut saw_done = false;
        while let Some(event) = stream.next_event().await {
            match event {
                AssistantMessageEvent::Error { .. } => saw_error = true,
                AssistantMessageEvent::Done { .. } => saw_done = true,
                _ => {}
            }
        }
        let message = stream.result().await;
        handle.await.unwrap();

        assert!(saw_error);
        assert!(!saw_done);
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 4h52m9s.")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    const FAKE_ACP_SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$@" > "$dir/acp-args.txt"
u='{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acp-sess-1","update":'
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$dir/acp-requests.txt"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1}}\n' "$id" ;;
    *'"method":"session/new"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"acp-sess-1"}}\n' "$id" ;;
    *'"method":"session/load"'*)
      printf '%s%s}}\n' "$u" '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"replayed history"}}'
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
    *'"method":"session/set_config_option"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[]}}\n' "$id" ;;
    *'"method":"session/prompt"'*)
      printf '%s%s}}\n' "$u" '{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Reading the file."}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{"sessionId":"acp-sess-1","options":[{"optionId":"no","kind":"reject_once"},{"optionId":"yes-always","kind":"allow_always"}]}}'
      IFS= read -r reply
      printf '%s\n' "$reply" >> "$dir/acp-requests.txt"
      printf '%s%s}}\n' "$u" '{"sessionUpdate":"tool_call","toolCallId":"t1","name":"view_file","args":{"AbsolutePath":"/tmp/a.rs"},"status":"in_progress"}'
      printf '%s%s}}\n' "$u" '{"sessionUpdate":"tool_call_update","toolCallId":"t1","name":"view_file","status":"completed"}'
      printf '%s%s}}\n' "$u" '{"sessionUpdate":"usage_update","usage":{"inputTokens":9,"outputTokens":1,"totalTokens":10}}'
      printf '%s%s}}\n' "$u" '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello "}}'
      printf '%s%s}}\n' "$u" '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ACP!"}}'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","usage":{"inputTokens":9,"outputTokens":3,"cachedReadTokens":1,"totalTokens":13}}}\n' "$id" ;;
  esac
done
"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn acp_streams_thoughts_tools_text_and_resumes_session() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "pa-antigravity-acp-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("fake-agy-acp");
        std::fs::write(&script_path, FAKE_ACP_SCRIPT).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let neon = builtin_models()
            .into_iter()
            .find(|m| m.id == "gemini-4-neon")
            .unwrap();
        let user = |text: &str| {
            Message::User(radiata_types::ai::UserMessage {
                content: UserMessageContent::Text(text.to_string()),
                timestamp: now_ms(),
                rest: Map::default(),
            })
        };
        let run = |context: Context| {
            let (writer, stream) = create_assistant_message_event_stream();
            let handle = tokio::spawn(run_stream_with_bin(
                writer,
                neon.clone(),
                context,
                StreamOptions {
                    session_id: Some("test-session-agy-acp".to_string()),
                    ..Default::default()
                },
                Some(ModelThinkingLevel::High),
                Some(script_path.clone()),
            ));
            (stream, handle)
        };

        let first_context = Context {
            system_prompt: None,
            messages: vec![user("Read a.rs")],
            tools: None,
        };
        let (mut stream, handle) = run(first_context.clone());
        let mut order = Vec::new();
        let mut first_text_cost = None;
        while let Some(event) = stream.next_event().await {
            match event {
                AssistantMessageEvent::ThinkingStart { .. } => order.push("thinking_start".into()),
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    order.push(format!("thinking:{delta}"));
                }
                AssistantMessageEvent::ThinkingEnd { .. } => order.push("thinking_end".into()),
                AssistantMessageEvent::TextDelta { delta, partial, .. } => {
                    first_text_cost.get_or_insert(partial.usage.cost.total.as_f64());
                    order.push(format!("text:{delta}"))
                }
                AssistantMessageEvent::TextEnd { content, .. } => {
                    order.push(format!("text_end:{content}"));
                }
                _ => {}
            }
        }
        let message = stream.result().await;
        handle.await.unwrap();

        assert_eq!(
            order,
            vec![
                "thinking_start".to_string(),
                "thinking:Reading the file.".to_string(),
                "thinking:\n▸ view_file /tmp/a.rs\n".to_string(),
                "thinking_end".to_string(),
                "text:Hello ".to_string(),
                "text:ACP!".to_string(),
                "text_end:Hello ACP!".to_string(),
            ]
        );
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.response_id.as_deref(), Some("acp-sess-1"));
        assert_eq!(message.usage.input, 9);
        assert_eq!(message.usage.output, 3);
        assert_eq!(message.usage.cache_read, 1);
        assert_eq!(message.usage.total_tokens, 13);
        // Gemini 4 Neon list price: $2.50 in, $15 out, $0.25 cache read per million tokens.
        let streamed = first_text_cost.expect("text streamed");
        assert!(
            (streamed - 37.5e-6).abs() < 1e-12,
            "streamed cost {streamed}"
        );
        assert!((message.usage.cost.total.as_f64() - 67.75e-6).abs() < 1e-12);

        let args = std::fs::read_to_string(dir.join("acp-args.txt")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            ["acp", "--dangerously-skip-permissions"]
        );
        let requests: Vec<Value> = std::fs::read_to_string(dir.join("acp-requests.txt"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let config: Vec<(&str, &str)> = requests
            .iter()
            .filter(|r| r["method"] == "session/set_config_option")
            .map(|r| {
                (
                    r["params"]["configId"].as_str().unwrap(),
                    r["params"]["value"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            config,
            [("model", "gemini-4-neon"), ("thought_level", "high")]
        );
        let permission = requests
            .iter()
            .find(|r| r["id"] == "perm-1")
            .expect("permission reply");
        assert_eq!(permission["result"]["outcome"]["optionId"], "yes-always");
        let prompt = requests
            .iter()
            .find(|r| r["method"] == "session/prompt")
            .unwrap();
        assert_eq!(prompt["params"]["prompt"][0]["text"], "Read a.rs");

        let mut second_context = first_context;
        second_context
            .messages
            .push(Message::Assistant(message.clone()));
        second_context.messages.push(user("again"));
        let (mut stream, handle) = run(second_context.clone());
        let mut texts = Vec::new();
        while let Some(event) = stream.next_event().await {
            if let AssistantMessageEvent::TextDelta { delta, .. } = event {
                texts.push(delta);
            }
        }
        let second = stream.result().await;
        handle.await.unwrap();
        assert_eq!(second.stop_reason, StopReason::Stop);
        assert_eq!(texts, vec!["Hello ", "ACP!"]);

        let requests: Vec<Value> = std::fs::read_to_string(dir.join("acp-requests.txt"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        // Second turn reuses the warm persistent ACP process: no second initialize or session/load!
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "session/load")
                .count(),
            0
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "session/new")
                .count(),
            1
        );
        let last_prompt = requests
            .iter()
            .rev()
            .find(|r| r["method"] == "session/prompt")
            .unwrap();
        assert_eq!(last_prompt["params"]["prompt"][0]["text"], "again");

        // After pool eviction (e.g. idle timeout or restart), the third turn falls back cleanly to session/load.
        clear_warm_acp_pool().await;
        let mut third_context = second_context;
        third_context.messages.push(Message::Assistant(second));
        third_context.messages.push(user("third"));
        let (mut stream, handle) = run(third_context);
        while stream.next_event().await.is_some() {}
        let third = stream.result().await;
        handle.await.unwrap();
        assert_eq!(third.stop_reason, StopReason::Stop);

        let requests: Vec<Value> = std::fs::read_to_string(dir.join("acp-requests.txt"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let load = requests
            .iter()
            .find(|r| r["method"] == "session/load")
            .expect("evicted turn resumes via session/load");
        assert_eq!(load["params"]["sessionId"], "acp-sess-1");
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "session/new")
                .count(),
            1
        );
        let last_prompt = requests
            .iter()
            .rev()
            .find(|r| r["method"] == "session/prompt")
            .unwrap();
        assert_eq!(last_prompt["params"]["prompt"][0]["text"], "third");

        clear_warm_acp_pool().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_quota_banner_as_error_text() {
        let raw = "\n⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 4h52m9s.";
        assert_eq!(
            detect_quota_or_auth_banner(raw).as_deref(),
            Some("⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 4h52m9s.")
        );
        assert!(detect_quota_or_auth_banner("Invalid API key · Fix external API key").is_some());
    }

    #[test]
    fn ignores_long_answers_that_mention_quota_wording() {
        assert_eq!(detect_quota_or_auth_banner(""), None);
        assert_eq!(detect_quota_or_auth_banner("  \n "), None);
        assert_eq!(
            detect_quota_or_auth_banner("Here is the refactored function."),
            None
        );
        let docs = format!(
            "The client retries when the server answers `quota exceeded`.\n\n```rust\n{}\n```",
            "if status == 429 { return Err(Error::QuotaExceeded(\"quota exceeded\".into())); }\n"
                .repeat(8)
        );
        assert!(docs.len() > BANNER_MAX_LEN);
        assert_eq!(detect_quota_or_auth_banner(&docs), None);
    }

    #[test]
    fn tool_progress_line_picks_primary_argument() {
        assert_eq!(
            tool_progress_line(
                "run_command",
                &serde_json::json!({"CommandLine": "cargo test\nmore", "Cwd": "/x"})
            ),
            "\n▸ run_command cargo test\n"
        );
        assert_eq!(
            tool_progress_line("view_file", &serde_json::json!({"AbsolutePath": "/a/b.rs"})),
            "\n▸ view_file /a/b.rs\n"
        );
        assert_eq!(tool_progress_line("finish", &Value::Null), "\n▸ finish\n");
        assert_eq!(
            tool_progress_line(
                "schedule",
                &serde_json::json!({"DurationSeconds": 480, "Prompt": "check job status"})
            ),
            "\n▸ schedule 8m · check job status\n"
        );
        assert_eq!(
            tool_progress_line("schedule", &serde_json::json!({"DurationSeconds": 95})),
            "\n▸ schedule 1m 35s\n"
        );
    }

    #[test]
    fn parses_antigravity_auth_models_status() {
        let ok = parse_auth_models_status(
            "Fetching available models...\ngemini-4-neon-high\tGemini 4 Neon (High)\ngemini-4-neon-low\tGemini 4 Neon (Low)\n",
            "",
            true,
        );
        assert!(ok.logged_in);
        assert_eq!(ok.model_count, 1);
        assert_eq!(ok.detail, None);

        let unauth = parse_auth_models_status(
            "Fetching available models...\n",
            "Not signed in. Run `agy` to sign in.\n",
            false,
        );
        assert!(!unauth.logged_in);
        assert_eq!(unauth.model_count, 0);
        assert_eq!(
            unauth.detail.as_deref(),
            Some("Not signed in. Run `agy` to sign in.")
        );
    }
}
