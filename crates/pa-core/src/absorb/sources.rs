//! Layer 0: deterministic transcript parsing for every local provider. Keeps
//! user text, the last assistant text of each turn, one line per tool call,
//! clipped error results, file operations, commands, and native compaction
//! summaries; drops thinking, duplicate reads, and bulky tool output.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{canonical, estimate_tokens, parse_timestamp, SourceRef};

const MAX_RECORD: usize = 16 * 1024 * 1024;
const MAX_FILE: u64 = 128 * 1024 * 1024;
/// Error results keep at most this many characters (head and tail).
pub const MAX_ERROR_CHARS: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceProvider {
    Prime,
    Claude,
    Codex,
    Cursor,
    Antigravity,
    T3,
    Pa,
}

impl SourceProvider {
    /// # Errors
    ///
    /// Fails on an unknown provider name.
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name.trim().to_ascii_lowercase().as_str() {
            "prime" => Self::Prime,
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "cursor" => Self::Cursor,
            "antigravity" => Self::Antigravity,
            "t3" | "flock" => Self::T3,
            "pa" => Self::Pa,
            other => bail!("Unknown provider {other}"),
        })
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prime => "prime",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Antigravity => "antigravity",
            Self::T3 => "t3",
            Self::Pa => "pa",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Prime => "Prime",
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::Antigravity => "Antigravity",
            Self::T3 => "T3",
            Self::Pa => "pa thread",
        }
    }
}

/// A source resolved to the file it is read from. T3 threads live in a
/// shared SQLite database, so they also carry the thread id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSource {
    pub provider: SourceProvider,
    pub path: PathBuf,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommandRecord {
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
    #[serde(default)]
    pub failed: bool,
    pub turn: u32,
}

impl CommandRecord {
    /// `pass`, `fail`, `exit 101`, or empty when unknown.
    #[must_use]
    pub fn status(&self) -> String {
        match self.exit {
            Some(0) => "pass".to_owned(),
            Some(code) => format!("exit {code}"),
            None if self.failed => "fail".to_owned(),
            None => String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DistilledTurn {
    pub index: u32,
    pub user: String,
    pub assistant: String,
    pub tools: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DistilledSource {
    pub provider: Option<SourceProvider>,
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub head_commit: Option<String>,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub turns: Vec<DistilledTurn>,
    pub raw_bytes: u64,
    pub raw_text_tokens: u64,
    pub files_modified: BTreeSet<String>,
    pub files_read: BTreeSet<String>,
    /// False when the provider records no tool calls (T3, pa threads).
    pub files_known: bool,
    pub commands: Vec<CommandRecord>,
    pub commits: Vec<String>,
    /// The source's own compaction summary, when it has one.
    pub existing_summary: Option<String>,
    /// Turns that precede `existing_summary` (the tail starts here).
    pub summary_turn_index: usize,
    /// Rows Layer 0 dropped (thinking, meta, tool output, abandoned branches).
    pub stripped_turns: u64,
    pub warnings: Vec<String>,
}

impl DistilledSource {
    /// Tokens of the turns after the native summary.
    #[must_use]
    pub fn tail_tokens(&self) -> u64 {
        self.turns
            .iter()
            .skip(self.summary_turn_index)
            .map(|turn| estimate_tokens(&turn.user) + estimate_tokens(&turn.assistant))
            .sum()
    }

    #[must_use]
    pub fn provider(&self) -> SourceProvider {
        self.provider.unwrap_or(SourceProvider::Prime)
    }
}

/// Resolve a provider and a path or thread id to the file to read.
///
/// # Errors
///
/// Fails on an unknown provider or a missing transcript.
pub fn resolve(source: &SourceRef, agent_dir: &Path) -> Result<ResolvedSource> {
    let provider = SourceProvider::parse(&source.provider)?;
    let location = source.location.trim();
    ensure!(
        !location.is_empty(),
        "Name the {} chat to absorb",
        provider.as_str()
    );
    let given = Path::new(location);
    let resolved = match provider {
        SourceProvider::T3 => ResolvedSource {
            provider,
            path: if given.is_file() {
                given.to_path_buf()
            } else {
                t3_database()
            },
            thread_id: Some(if given.is_file() {
                bail!("Name a T3 thread by its id")
            } else {
                location.to_owned()
            }),
        },
        SourceProvider::Pa if !given.is_file() => {
            let id =
                uuid::Uuid::parse_str(location).context("Name a pa thread by its id or file")?;
            ResolvedSource {
                provider,
                path: agent_dir.join("threads").join(format!("{id}.json")),
                thread_id: Some(id.to_string()),
            }
        }
        _ => ResolvedSource {
            provider,
            path: canonical(given),
            thread_id: None,
        },
    };
    ensure!(
        resolved.path.is_file(),
        "No {} transcript at {}",
        provider.as_str(),
        resolved.path.display()
    );
    Ok(resolved)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn t3_database() -> PathBuf {
    home().join(".t3/userdata/statev2.sqlite")
}

/// Parse a resolved source.
///
/// # Errors
///
/// Fails when the transcript is unreadable, oversized, or has no messages.
pub fn parse(source: &ResolvedSource) -> Result<DistilledSource> {
    let meta = std::fs::metadata(&source.path)
        .with_context(|| format!("Missing transcript {}", source.path.display()))?;
    let mut out = match source.provider {
        SourceProvider::T3 => parse_t3(source)?,
        SourceProvider::Pa => parse_pa(&source.path)?,
        provider => {
            ensure!(
                meta.is_file() && meta.len() <= MAX_FILE,
                "Select a regular conversation file of at most 128 MiB"
            );
            parse_jsonl(provider, &source.path)?
        }
    };
    out.provider = Some(source.provider);
    out.path.clone_from(&source.path);
    if source.provider != SourceProvider::T3 {
        out.raw_bytes = meta.len();
    }
    if out.ended_ms == 0 {
        out.ended_ms = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_millis() as u64);
    }
    if out.started_ms == 0 {
        out.started_ms = out.ended_ms;
    }
    if out.title.trim().is_empty() {
        out.title = out
            .turns
            .iter()
            .find(|turn| !turn.user.is_empty())
            .map(|turn| one_line(&turn.user, 80))
            .unwrap_or_else(|| "Untitled chat".to_owned());
    }
    ensure!(
        !out.turns.is_empty() || out.existing_summary.is_some(),
        "Conversation has no visible user or assistant messages"
    );
    Ok(out)
}

/// One normalized transcript event, in conversation order.
#[derive(Debug, Clone)]
enum Event {
    User(String),
    Assistant(String),
    Tool {
        id: Option<String>,
        name: String,
        input: Value,
    },
    Result {
        id: Option<String>,
        error: bool,
        text: String,
        exit: Option<i64>,
        diffs: Vec<String>,
    },
    Summary(String),
    Stripped,
}

/// A row of a tree-shaped transcript (Prime, Claude): the active branch is the
/// parent walk from the last row.
struct Row {
    id: String,
    parent: Option<String>,
    events: Vec<Event>,
}

#[derive(Default)]
struct Meta {
    id: Option<String>,
    title: Option<String>,
    cwd: Option<PathBuf>,
    git_branch: Option<String>,
    head_commit: Option<String>,
    started_ms: u64,
    ended_ms: u64,
    warnings: Vec<String>,
}

impl Meta {
    fn stamp(&mut self, value: &Value) {
        if let Some(ms) = parse_timestamp(value) {
            if self.started_ms == 0 || ms < self.started_ms {
                self.started_ms = ms;
            }
            self.ended_ms = self.ended_ms.max(ms);
        }
    }

    fn set_cwd(&mut self, value: &Value) {
        if let Some(dir) = value.as_str().filter(|dir| Path::new(dir).is_absolute()) {
            self.cwd = Some(PathBuf::from(dir));
        }
    }
}

fn records(input: impl Read, mut visit: impl FnMut(Value) -> Result<()>) -> Result<()> {
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader
            .by_ref()
            .take(MAX_RECORD as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        ensure!(n <= MAX_RECORD, "Provider record exceeds 16 MiB");
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // A live transcript can end in a half-written line.
        if let Ok(value) = serde_json::from_slice(&line) {
            visit(value)?;
        }
    }
    Ok(())
}

fn parse_jsonl(provider: SourceProvider, path: &Path) -> Result<DistilledSource> {
    let file = File::open(path).context("Could not open the conversation")?;
    let mut meta = Meta::default();
    let mut rows: Vec<Row> = Vec::new();
    let mut linear: Vec<Event> = Vec::new();
    let mut stripped = 0u64;
    let mut seen_ids = HashSet::new();
    records(file, |event| {
        let kind = event["type"].as_str().unwrap_or_default();
        match provider {
            SourceProvider::Prime => {
                prime_record(&event, kind, &mut meta, &mut rows, &mut seen_ids)
            }
            SourceProvider::Claude => {
                claude_record(&event, kind, &mut meta, &mut rows, &mut seen_ids)
            }
            SourceProvider::Codex => {
                codex_record(&event, kind, &mut meta, &mut linear);
                Ok(())
            }
            SourceProvider::Cursor => {
                cursor_record(&event, kind, &mut meta, &mut linear);
                Ok(())
            }
            SourceProvider::Antigravity => {
                antigravity_record(&event, kind, &mut meta, &mut linear);
                Ok(())
            }
            SourceProvider::T3 | SourceProvider::Pa => unreachable!(),
        }
    })?;
    let events = if matches!(provider, SourceProvider::Prime | SourceProvider::Claude) {
        active_branch(rows, &mut meta.warnings, &mut stripped)
    } else {
        linear
    };
    if matches!(
        provider,
        SourceProvider::Cursor | SourceProvider::Antigravity
    ) && meta.id.is_none()
    {
        meta.id = id_from_path(path);
    }
    let mut out = build(events, &meta);
    out.stripped_turns += stripped;
    if out.id.is_empty() {
        out.id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    Ok(out)
}

/// The parent walk from the last row; abandoned siblings drop out. A broken
/// chain falls back to file order with a warning.
fn active_branch(rows: Vec<Row>, warnings: &mut Vec<String>, stripped: &mut u64) -> Vec<Event> {
    let Some(last) = rows.last() else {
        return Vec::new();
    };
    let index: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| (row.id.as_str(), i))
        .collect();
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = Some(last.id.as_str());
    let mut broken = false;
    while let Some(id) = cursor {
        let Some(&at) = index.get(id) else {
            broken = true;
            break;
        };
        if !seen.insert(at) {
            broken = true;
            break;
        }
        chain.push(at);
        cursor = rows[at].parent.as_deref();
    }
    if broken {
        warnings.push(
            "Could not resolve the active branch; read the transcript in file order".to_owned(),
        );
        return rows.into_iter().flat_map(|row| row.events).collect();
    }
    chain.reverse();
    let keep: HashSet<usize> = chain.iter().copied().collect();
    let mut rows: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
    for (i, row) in rows.iter().enumerate() {
        if !keep.contains(&i) {
            if let Some(row) = row {
                *stripped += row.events.len() as u64;
            }
        }
    }
    chain
        .into_iter()
        .filter_map(|i| rows[i].take())
        .flat_map(|row| row.events)
        .collect()
}

fn prime_record(
    event: &Value,
    kind: &str,
    meta: &mut Meta,
    rows: &mut Vec<Row>,
    seen: &mut HashSet<String>,
) -> Result<()> {
    meta.stamp(&event["timestamp"]);
    if kind == "session" {
        meta.id = event["id"].as_str().map(str::to_owned);
        meta.set_cwd(&event["cwd"]);
        return Ok(());
    }
    if kind == "session_info" {
        if let Some(name) = event["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
        {
            meta.title = Some(name.to_owned());
        }
    }
    let Some(id) = event["id"].as_str() else {
        return Ok(());
    };
    ensure!(
        seen.insert(id.to_owned()),
        "Duplicate conversation entry ID"
    );
    let mut events = Vec::new();
    match kind {
        "message" => {
            let message = &event["message"];
            match message["role"].as_str() {
                Some("user") => {
                    if let Some(text) = user_text(&message["content"]) {
                        events.push(Event::User(text));
                    }
                }
                Some("assistant") => assistant_blocks(&message["content"], &mut events),
                Some("toolResult") => {
                    let diffs = message["details"]["diffs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|diff| diff["path"].as_str().filter(|p| !p.is_empty()))
                        .map(str::to_owned)
                        .collect();
                    let text = joined_text(&message["content"]);
                    events.push(Event::Result {
                        id: message["toolCallId"].as_str().map(str::to_owned),
                        error: message["isError"] == true,
                        exit: exit_code_in(&text),
                        text,
                        diffs,
                    });
                }
                _ => events.push(Event::Stripped),
            }
        }
        "compaction" => {
            if let Some(summary) = event["summary"].as_str().filter(|s| !s.trim().is_empty()) {
                events.push(Event::Summary(summary.to_owned()));
                for (field, modified) in [("modifiedFiles", true), ("readFiles", false)] {
                    for path in event["details"][field]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        events.push(Event::Tool {
                            id: None,
                            name: if modified { "edit" } else { "read" }.to_owned(),
                            input: serde_json::json!({ "path": path, "silent": true }),
                        });
                    }
                }
            }
        }
        _ => {}
    }
    rows.push(Row {
        id: id.to_owned(),
        parent: event["parentId"].as_str().map(str::to_owned),
        events,
    });
    Ok(())
}

fn claude_record(
    event: &Value,
    kind: &str,
    meta: &mut Meta,
    rows: &mut Vec<Row>,
    seen: &mut HashSet<String>,
) -> Result<()> {
    if event["isSidechain"] == true {
        return Ok(());
    }
    if kind == "summary" {
        if let Some(summary) = event["summary"].as_str() {
            meta.title.get_or_insert_with(|| summary.to_owned());
        }
        return Ok(());
    }
    if kind == "custom-title" {
        if let Some(title) = event["customTitle"].as_str() {
            meta.title = Some(title.to_owned());
        }
        return Ok(());
    }
    let Some(id) = event["uuid"].as_str() else {
        return Ok(());
    };
    ensure!(
        seen.insert(id.to_owned()),
        "Duplicate conversation entry ID"
    );
    meta.stamp(&event["timestamp"]);
    if let Some(session) = event["sessionId"].as_str() {
        meta.id.get_or_insert_with(|| session.to_owned());
    }
    meta.set_cwd(&event["cwd"]);
    if let Some(branch) = event["gitBranch"]
        .as_str()
        .filter(|b| !b.is_empty() && *b != "HEAD")
    {
        meta.git_branch = Some(branch.to_owned());
    }
    let mut events = Vec::new();
    let message = &event["message"];
    match kind {
        "user" if event["isCompactSummary"] == true => {
            if let Some(text) = user_text(&message["content"]) {
                events.push(Event::Summary(text));
            }
        }
        "user" if event["isMeta"] == true => events.push(Event::Stripped),
        "user" => {
            if let Some(blocks) = message["content"].as_array() {
                for block in blocks {
                    if block["type"] == "tool_result" {
                        let text = joined_text(&block["content"]);
                        events.push(Event::Result {
                            id: block["tool_use_id"].as_str().map(str::to_owned),
                            error: block["is_error"] == true,
                            exit: exit_code_in(&text),
                            text,
                            diffs: Vec::new(),
                        });
                    }
                }
            }
            if let Some(text) = user_text(&message["content"]) {
                events.push(Event::User(text));
            }
        }
        "assistant" => assistant_blocks(&message["content"], &mut events),
        _ => {}
    }
    // A compact boundary restarts the chain (`parentUuid: null`); its logical
    // parent keeps the earlier turns on the active branch.
    let parent = event["parentUuid"]
        .as_str()
        .or_else(|| event["logicalParentUuid"].as_str())
        .map(str::to_owned);
    rows.push(Row {
        id: id.to_owned(),
        parent,
        events,
    });
    Ok(())
}

fn codex_record(event: &Value, kind: &str, meta: &mut Meta, events: &mut Vec<Event>) {
    meta.stamp(&event["timestamp"]);
    let payload = &event["payload"];
    match kind {
        "session_meta" => {
            if let Some(id) = payload["id"]
                .as_str()
                .or_else(|| payload["session_id"].as_str())
            {
                meta.id = Some(id.to_owned());
            }
            meta.set_cwd(&payload["cwd"]);
            if let Some(branch) = payload["git"]["branch"].as_str() {
                meta.git_branch = Some(branch.to_owned());
            }
            if let Some(commit) = payload["git"]["commit_hash"].as_str() {
                meta.head_commit = Some(commit.chars().take(12).collect());
            }
        }
        "turn_context" => meta.set_cwd(&payload["cwd"]),
        "compacted" => {
            if let Some(text) = payload["message"].as_str().filter(|s| !s.trim().is_empty()) {
                events.push(Event::Summary(text.to_owned()));
            }
        }
        "event_msg"
            if matches!(
                payload["type"].as_str(),
                Some("thread_rolled_back" | "thread_rollback" | "rollback")
            ) =>
        {
            let turns = payload["num_turns"].as_u64().unwrap_or(0);
            rollback(events, turns);
            meta.warnings.push(if turns > 0 {
                format!("The Codex chat rolled back {turns} turn(s); only the active turns are absorbed")
            } else {
                "The Codex chat rolled back; only the turns after the rollback are absorbed".to_owned()
            });
        }
        "response_item" => match payload["type"].as_str() {
            Some("message") => match payload["role"].as_str() {
                Some("user") => {
                    if let Some(text) = user_text(&payload["content"]) {
                        if !is_codex_preamble(&text) {
                            events.push(Event::User(text));
                        }
                    }
                }
                Some("assistant") => {
                    let text = joined_text(&payload["content"]);
                    if !text.trim().is_empty() {
                        events.push(Event::Assistant(text.trim().to_owned()));
                    }
                }
                _ => events.push(Event::Stripped),
            },
            Some("function_call" | "custom_tool_call" | "local_shell_call") => {
                let input = match (&payload["arguments"], &payload["input"]) {
                    (Value::String(raw), _) => {
                        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
                    }
                    (_, Value::String(raw)) => serde_json::json!({ "patch": raw }),
                    (args, _) if args.is_object() => args.clone(),
                    _ => payload["action"].clone(),
                };
                events.push(Event::Tool {
                    id: payload["call_id"].as_str().map(str::to_owned),
                    name: payload["name"].as_str().unwrap_or("shell").to_owned(),
                    input,
                });
            }
            Some(
                "function_call_output" | "custom_tool_call_output" | "local_shell_call_output",
            ) => {
                let (text, exit) = codex_output(&payload["output"]);
                events.push(Event::Result {
                    id: payload["call_id"].as_str().map(str::to_owned),
                    error: exit.is_some_and(|code| code != 0),
                    text,
                    exit,
                    diffs: Vec::new(),
                });
            }
            _ => events.push(Event::Stripped),
        },
        _ => {}
    }
}

/// Drop the last `turns` user turns (all events when the count is unknown).
fn rollback(events: &mut Vec<Event>, turns: u64) {
    if turns == 0 {
        events.clear();
        return;
    }
    let mut remaining = turns;
    while let Some(at) = events
        .iter()
        .rposition(|event| matches!(event, Event::User(_)))
    {
        events.truncate(at);
        remaining -= 1;
        if remaining == 0 {
            return;
        }
    }
    events.clear();
}

fn is_codex_preamble(text: &str) -> bool {
    let text = text.trim_start();
    [
        "<environment_context>",
        "<user_instructions>",
        "# AGENTS.md instructions",
        "<permissions instructions>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn codex_output(output: &Value) -> (String, Option<i64>) {
    let raw = match output {
        Value::String(text) => text.clone(),
        Value::Object(_) => output["content"]
            .as_str()
            .map_or_else(|| output.to_string(), str::to_owned),
        _ => String::new(),
    };
    if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
        if parsed.is_object() {
            let exit = parsed["metadata"]["exit_code"].as_i64();
            let text = parsed["output"].as_str().unwrap_or_default().to_owned();
            return (text, exit);
        }
    }
    let exit = exit_code_in(&raw);
    (raw, exit)
}

fn cursor_record(event: &Value, kind: &str, meta: &mut Meta, events: &mut Vec<Event>) {
    meta.stamp(&event["timestamp"]);
    meta.set_cwd(&event["cwd"]);
    let role = event["role"]
        .as_str()
        .or_else(|| event["message"]["role"].as_str())
        .unwrap_or(kind);
    let content = if event["message"].is_object() {
        &event["message"]["content"]
    } else {
        &event["content"]
    };
    match role {
        "user" => {
            if let Some(text) = user_text(content) {
                let text = strip_tag(&text, "user_query");
                if !text.is_empty() {
                    events.push(Event::User(text.to_owned()));
                }
            }
        }
        "assistant" => assistant_blocks(content, events),
        _ => events.push(Event::Stripped),
    }
}

fn antigravity_record(event: &Value, kind: &str, meta: &mut Meta, events: &mut Vec<Event>) {
    meta.stamp(&event["created_at"]);
    meta.stamp(&event["timestamp"]);
    if let Some(id) = event["conversation_id"].as_str() {
        meta.id.get_or_insert_with(|| id.to_owned());
    }
    meta.set_cwd(&event["cwd"]);
    match kind {
        "USER_INPUT" => {
            if let Some(body) = event["content"].as_str() {
                let cleaned = strip_tag(body, "USER_REQUEST");
                if !cleaned.is_empty() {
                    events.push(Event::User(cleaned.to_owned()));
                }
            }
        }
        "PLANNER_RESPONSE" => {
            if let Some(body) = event["content"].as_str().filter(|b| !b.trim().is_empty()) {
                events.push(Event::Assistant(body.trim().to_owned()));
            }
            for call in event["tool_calls"].as_array().into_iter().flatten() {
                let input = match &call["args"] {
                    Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
                    Value::Null => call["arguments"].clone(),
                    args => args.clone(),
                };
                events.push(Event::Tool {
                    id: call["id"].as_str().map(str::to_owned),
                    name: call["name"].as_str().unwrap_or("tool").to_owned(),
                    input,
                });
            }
        }
        "" => {}
        step => {
            // Tool steps name their file or command in the step's own fields.
            let fields = if event["metadata"].is_object() {
                &event["metadata"]
            } else {
                event
            };
            if path_of(fields).is_some() || command_of(fields).is_some() {
                events.push(Event::Tool {
                    id: None,
                    name: step.to_ascii_lowercase(),
                    input: fields.clone(),
                });
            } else {
                events.push(Event::Stripped);
            }
        }
    }
}

fn parse_pa(path: &Path) -> Result<DistilledSource> {
    let thread: Value =
        serde_json::from_slice(&std::fs::read(path)?).context("Invalid pa thread file")?;
    let mut meta = Meta {
        id: thread["id"].as_str().map(str::to_owned),
        ..Meta::default()
    };
    meta.set_cwd(&thread["cwd"]);
    let mut events = Vec::new();
    for pair in thread["messages"].as_array().into_iter().flatten() {
        let (Some(role), Some(text)) = (pair[0].as_str(), pair[1].as_str()) else {
            continue;
        };
        match role {
            "user" => events.push(Event::User(text.to_owned())),
            "assistant" => events.push(Event::Assistant(text.to_owned())),
            _ => {}
        }
    }
    let mut out = build(events, &meta);
    out.files_known = false;
    Ok(out)
}

/// T3 threads live in `statev2.sqlite`; read them read-only with `sqlite3`.
fn parse_t3(source: &ResolvedSource) -> Result<DistilledSource> {
    let id = source
        .thread_id
        .as_deref()
        .context("Name a T3 thread by its id")?;
    ensure!(
        id.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
        "Invalid T3 thread id"
    );
    let db = &source.path;
    let tables = sqlite_json(db, "SELECT name FROM sqlite_master WHERE type='table'")?;
    let has = |name: &str| tables.iter().any(|row| row["name"] == name);
    let modern = has("orchestration_v2_projection_threads");
    let (threads, messages, text, directory) = if modern {
        (
            "orchestration_v2_projection_threads",
            "orchestration_v2_projection_messages",
            "json_extract(payload_json,'$.text')",
            "json_extract(t.payload_json,'$.worktreePath')",
        )
    } else {
        ensure!(
            has("projection_threads"),
            "No T3 threads in {}",
            db.display()
        );
        (
            "projection_threads",
            "projection_thread_messages",
            "text",
            "t.worktree_path",
        )
    };
    let projects = if has("projection_projects") {
        format!("coalesce({directory},p.workspace_root)")
    } else {
        directory.to_owned()
    };
    let join = if has("projection_projects") {
        "LEFT JOIN projection_projects p ON p.project_id=t.project_id"
    } else {
        ""
    };
    let head = sqlite_json(
        db,
        &format!("SELECT t.title AS title, {projects} AS cwd, t.updated_at AS updated FROM {threads} t {join} WHERE t.thread_id='{id}'"),
    )?;
    let head = head.first().context("No such T3 thread")?;
    let rows = sqlite_json(
        db,
        &format!("SELECT role, {text} AS text, created_at AS at FROM {messages} WHERE thread_id='{id}' AND role IN ('user','assistant') ORDER BY created_at, rowid"),
    )?;
    let mut meta = Meta {
        id: Some(id.to_owned()),
        title: head["title"].as_str().map(str::to_owned),
        ..Meta::default()
    };
    meta.set_cwd(&head["cwd"]);
    meta.stamp(&head["updated"]);
    let mut events = Vec::new();
    for row in &rows {
        meta.stamp(&row["at"]);
        let Some(text) = row["text"].as_str().filter(|t| !t.trim().is_empty()) else {
            continue;
        };
        events.push(if row["role"] == "user" {
            Event::User(text.to_owned())
        } else {
            Event::Assistant(text.to_owned())
        });
    }
    let mut out = build(events, &meta);
    out.files_known = false;
    Ok(out)
}

fn sqlite_json(db: &Path, query: &str) -> Result<Vec<Value>> {
    let uri = format!("file:{}?mode=ro", db.display());
    let output = Command::new("sqlite3")
        .args(["-readonly", "-json", &uri, query])
        .output()
        .context("Reading T3 threads requires sqlite3")?;
    ensure!(
        output.status.success(),
        "sqlite3 failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&text).context("Invalid sqlite3 output")?)
}

/// What a tool call did, by tool name and input shape.
enum Effect {
    Read(String),
    Modify(Vec<String>),
    Command(String),
    Other,
}

fn path_of(input: &Value) -> Option<String> {
    [
        "file_path",
        "path",
        "target_file",
        "notebook_path",
        "filePath",
        "filename",
        "AbsolutePath",
        "TargetFile",
        "absolute_path",
        "file",
    ]
    .iter()
    .find_map(|key| input[key].as_str().filter(|p| !p.trim().is_empty()))
    .map(str::to_owned)
}

fn command_of(input: &Value) -> Option<String> {
    let raw = ["command", "cmd", "CommandLine", "commandLine"]
        .iter()
        .map(|key| &input[key])
        .find(|value| !value.is_null())?;
    let text = match raw {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                [shell, flag, script] if shell.ends_with("sh") && flag.starts_with('-') => {
                    (*script).to_owned()
                }
                _ => parts.join(" "),
            }
        }
        _ => return None,
    };
    let text = normalize_command(&text);
    (!text.is_empty()).then_some(text)
}

fn normalize_command(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn patch_files(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
        })
        .map(|path| path.trim().to_owned())
        .collect()
}

/// Files a shell command reads (`cat`, `sed -n`, `nl`, `head`, `tail`).
fn shell_reads(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in cmd.split(['|', ';', '&']) {
        let words: Vec<&str> = part.split_whitespace().collect();
        let Some(program) = words.first() else {
            continue;
        };
        if !matches!(*program, "cat" | "sed" | "nl" | "head" | "tail" | "bat") {
            continue;
        }
        if let Some(path) = words.iter().skip(1).rev().find(|word| {
            !word.starts_with('-')
                && !word.starts_with('\'')
                && !word.starts_with('"')
                && word.contains(['.', '/'])
        }) {
            out.push((*path).to_owned());
        }
    }
    out
}

fn classify(name: &str, input: &Value) -> Effect {
    let lower = name.to_ascii_lowercase();
    if let Some(patch) = input["patch"].as_str().or_else(|| input["input"].as_str()) {
        if lower.contains("patch") || patch.contains("*** Begin Patch") {
            return Effect::Modify(patch_files(patch));
        }
    }
    let path = path_of(input);
    if matches!(
        lower.as_str(),
        "bash"
            | "shell"
            | "exec_command"
            | "run_terminal_cmd"
            | "run_command"
            | "terminal"
            | "local_shell"
            | "shell_command"
            | "container.exec"
    ) || (path.is_none() && command_of(input).is_some())
    {
        if let Some(cmd) = command_of(input) {
            if cmd.contains("*** Begin Patch") {
                return Effect::Modify(patch_files(&cmd));
            }
            return Effect::Command(cmd);
        }
    }
    let Some(path) = path else {
        return Effect::Other;
    };
    let modifies = [
        "edit",
        "write",
        "replace",
        "create",
        "notebook",
        "patch",
        "delete",
        "code_action",
        "save",
    ];
    if modifies.iter().any(|word| lower.contains(word)) {
        return Effect::Modify(vec![path]);
    }
    if ["read", "view", "open", "cat", "grep"]
        .iter()
        .any(|word| lower.contains(word))
    {
        return Effect::Read(path);
    }
    Effect::Other
}

/// Fold events into turns, file sets, and commands.
fn build(events: Vec<Event>, meta: &Meta) -> DistilledSource {
    let mut out = DistilledSource {
        id: meta.id.clone().unwrap_or_default(),
        title: meta.title.clone().unwrap_or_default(),
        cwd: meta.cwd.clone(),
        git_branch: meta.git_branch.clone(),
        head_commit: meta.head_commit.clone(),
        started_ms: meta.started_ms,
        ended_ms: meta.ended_ms,
        files_known: true,
        warnings: meta.warnings.clone(),
        ..DistilledSource::default()
    };
    let mut raw_chars = 0u64;
    let mut current: Option<DistilledTurn> = None;
    // Shell call id → its index in `commands`.
    let mut calls: HashMap<String, usize> = HashMap::new();
    let mut read_seen: HashSet<String> = HashSet::new();
    let mut commands: Vec<CommandRecord> = Vec::new();
    let turn_index = |turns: &Vec<DistilledTurn>, current: &Option<DistilledTurn>| {
        (turns.len() + usize::from(current.is_some())).max(1) as u32
    };
    let rel = |path: &str| relative_path(path, meta.cwd.as_deref());
    for event in events {
        match event {
            Event::User(text) => {
                raw_chars += text.chars().count() as u64;
                if let Some(turn) = current.take() {
                    out.turns.push(turn);
                }
                current = Some(DistilledTurn {
                    index: out.turns.len() as u32 + 1,
                    user: text,
                    ..DistilledTurn::default()
                });
            }
            Event::Assistant(text) => {
                raw_chars += text.chars().count() as u64;
                let turn = current.get_or_insert_with(|| DistilledTurn {
                    index: out.turns.len() as u32 + 1,
                    ..DistilledTurn::default()
                });
                turn.assistant = text;
            }
            Event::Summary(text) => {
                if let Some(turn) = current.take() {
                    out.turns.push(turn);
                }
                out.existing_summary = Some(text);
                out.summary_turn_index = out.turns.len();
            }
            Event::Stripped => out.stripped_turns += 1,
            Event::Tool { id, name, input } => {
                let silent = input["silent"] == true;
                let turn_no = turn_index(&out.turns, &current);
                let line = match classify(&name, &input) {
                    Effect::Read(path) => {
                        let path = rel(&path);
                        let first = read_seen.insert(path.clone());
                        out.files_read.insert(path.clone());
                        first.then(|| format!("→ read {path}"))
                    }
                    Effect::Modify(paths) => {
                        let paths: Vec<String> = paths.iter().map(|p| rel(p)).collect();
                        out.files_modified.extend(paths.iter().cloned());
                        Some(format!(
                            "→ {} {}",
                            name.to_ascii_lowercase(),
                            paths.join(", ")
                        ))
                    }
                    Effect::Command(cmd) => {
                        for path in shell_reads(&cmd) {
                            out.files_read.insert(rel(&path));
                        }
                        let record = CommandRecord {
                            cmd: clip(&cmd, 200),
                            turn: turn_no,
                            ..CommandRecord::default()
                        };
                        commands.push(record);
                        if let Some(id) = &id {
                            calls.insert(id.clone(), commands.len() - 1);
                        }
                        Some(format!(
                            "→ {} \"{}\"",
                            name.to_ascii_lowercase(),
                            clip(&cmd, 160)
                        ))
                    }
                    Effect::Other => Some(format!("→ {}", name.to_ascii_lowercase())),
                };
                if silent {
                    continue;
                }
                if let Some(line) = line {
                    let turn = current.get_or_insert_with(|| DistilledTurn {
                        index: out.turns.len() as u32 + 1,
                        ..DistilledTurn::default()
                    });
                    turn.tools.push(line);
                }
            }
            Event::Result {
                id,
                error,
                text,
                exit,
                diffs,
            } => {
                for path in diffs {
                    out.files_modified.insert(rel(&path));
                }
                let command = id.as_ref().and_then(|id| calls.get(id)).copied();
                if let Some(at) = command {
                    let record = &mut commands[at];
                    record.exit = exit.or(record.exit);
                    record.failed = error || exit.is_some_and(|code| code != 0);
                    if record.cmd.starts_with("git ") {
                        out.commits.extend(commit_hashes(&text));
                    }
                }
                if error || exit.is_some_and(|code| code != 0) {
                    let turn = current.get_or_insert_with(|| DistilledTurn {
                        index: out.turns.len() as u32 + 1,
                        ..DistilledTurn::default()
                    });
                    let suffix = exit
                        .map(|code| format!(" (exit {code})"))
                        .unwrap_or_default();
                    if let Some(last) = turn.tools.last_mut() {
                        if command.is_some() && !last.ends_with(')') {
                            last.push_str(&suffix);
                        }
                    }
                    let snippet = clip_middle(text.trim(), MAX_ERROR_CHARS);
                    if !snippet.is_empty() {
                        turn.errors
                            .push(format!("! {}", snippet.replace('\n', "\n  ")));
                    }
                } else {
                    out.stripped_turns += 1;
                }
            }
        }
    }
    if let Some(turn) = current.take() {
        out.turns.push(turn);
    }
    for (i, turn) in out.turns.iter_mut().enumerate() {
        turn.index = i as u32 + 1;
    }
    // Keep the last record of each command, in the order it last ran.
    let mut last: Vec<CommandRecord> = Vec::new();
    for record in commands.into_iter().rev() {
        if !last.iter().any(|kept| kept.cmd == record.cmd) {
            last.push(record);
        }
    }
    last.reverse();
    out.commands = last;
    for path in &out.files_modified {
        out.files_read.remove(path);
    }
    out.commits.sort();
    out.commits.dedup();
    out.raw_text_tokens = raw_chars.div_ceil(4);
    out
}

fn relative_path(path: &str, cwd: Option<&Path>) -> String {
    let path = path.trim();
    if let Some(cwd) = cwd {
        if let Ok(rest) = Path::new(path).strip_prefix(cwd) {
            let rest = rest.to_string_lossy();
            if !rest.is_empty() {
                return rest.into_owned();
            }
        }
    }
    path.trim_start_matches("./").to_owned()
}

fn commit_hashes(text: &str) -> Vec<String> {
    // `[branch 1b1b76e] message` from `git commit`.
    text.lines()
        .filter_map(|line| {
            let inner = line.trim().strip_prefix('[')?.split(']').next()?;
            let hash = inner.split_whitespace().last()?;
            (hash.len() >= 7 && hash.len() <= 40 && hash.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| hash.to_owned())
        })
        .collect()
}

fn exit_code_in(text: &str) -> Option<i64> {
    for marker in [
        "Exit code: ",
        "exit code: ",
        "Process exited with code ",
        "exit_code=",
    ] {
        if let Some(at) = text.find(marker) {
            let digits: String = text[at + marker.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '-')
                .collect();
            if let Ok(code) = digits.parse() {
                return Some(code);
            }
        }
    }
    None
}

fn assistant_blocks(content: &Value, events: &mut Vec<Event>) {
    match content {
        Value::String(text) if !text.trim().is_empty() => {
            events.push(Event::Assistant(text.trim().to_owned()))
        }
        Value::Array(blocks) => {
            let mut text = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text" | "output_text") => {
                        if let Some(body) = block["text"].as_str().filter(|b| !b.trim().is_empty())
                        {
                            text.push(body.trim().to_owned());
                        }
                    }
                    Some("tool_use" | "toolCall" | "tool_call" | "function_call") => {
                        let input = ["input", "arguments", "args"]
                            .iter()
                            .map(|key| &block[key])
                            .find(|value| !value.is_null())
                            .cloned()
                            .unwrap_or(Value::Null);
                        let input = match input {
                            Value::String(raw) => {
                                serde_json::from_str(&raw).unwrap_or(Value::String(raw))
                            }
                            other => other,
                        };
                        events.push(Event::Tool {
                            id: block["id"].as_str().map(str::to_owned),
                            name: block["name"].as_str().unwrap_or("tool").to_owned(),
                            input,
                        });
                    }
                    _ => events.push(Event::Stripped),
                }
            }
            if !text.is_empty() {
                events.push(Event::Assistant(text.join("\n")));
            }
        }
        _ => {}
    }
}

/// Visible user text: text blocks only, wrapper markup and reminders removed;
/// `None` for slash-command echoes and empty rows.
fn user_text(content: &Value) -> Option<String> {
    let raw = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| matches!(b["type"].as_str(), Some("text" | "input_text")))
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let cleaned = remove_blocks(&raw, "system-reminder");
    let trimmed = cleaned.trim();
    if trimmed.is_empty()
        || [
            "<command-name>",
            "<command-message>",
            "<local-command-stdout>",
            "<local-command-caveat>",
            "Caveat: ",
        ]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return None;
    }
    Some(trimmed.to_owned())
}

fn joined_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn remove_blocks(text: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        out.push_str(&rest[..start]);
        match rest[start..].find(&close) {
            Some(end) => rest = &rest[start + end + close.len()..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The text inside the first `<tag>...</tag>`, or all of `content`.
fn strip_tag<'a>(content: &'a str, tag: &str) -> &'a str {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    if let Some(start) = content.find(&open) {
        let after = &content[start + open.len()..];
        if let Some(end) = after.find(&close) {
            return after[..end].trim();
        }
    }
    content.trim()
}

fn id_from_path(path: &Path) -> Option<String> {
    path.components().rev().find_map(|component| {
        let text = component.as_os_str().to_str()?;
        let stem = text.strip_suffix(".jsonl").unwrap_or(text);
        uuid::Uuid::parse_str(stem).is_ok().then(|| stem.to_owned())
    })
}

/// At most `max` characters, with an ellipsis.
#[must_use]
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// At most `max` characters, keeping the head and the tail.
#[must_use]
pub fn clip_middle(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let head = max * 2 / 3;
    let tail = max - head - 3;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(count - tail).collect();
    format!("{start} … {end}")
}

/// The first line, clipped.
#[must_use]
pub fn one_line(text: &str, max: usize) -> String {
    clip(
        &normalize_command(text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")),
        max,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn jsonl(dir: &Path, name: &str, rows: &[Value]) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(&path).unwrap();
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
        path
    }

    fn parsed(provider: SourceProvider, path: &Path) -> DistilledSource {
        parse(&ResolvedSource {
            provider,
            path: path.to_path_buf(),
            thread_id: None,
        })
        .unwrap()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn prime_follows_the_leaf_and_reads_compaction_and_tool_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let path = jsonl(
            dir.path(),
            "s.jsonl",
            &[
                serde_json::json!({"type":"session","id":"p1","cwd":"/work","timestamp":"2026-11-22T14:00:00Z"}),
                serde_json::json!({"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"fix the bug"}}),
                serde_json::json!({"type":"message","id":"b","parentId":"a","message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}]}}),
                serde_json::json!({"type":"compaction","id":"c","parentId":"a","summary":"## Goal\nfix","details":{"readFiles":["/work/src/lib.rs"],"modifiedFiles":[]}}),
                serde_json::json!({"type":"message","id":"d","parentId":"c","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"toolCall","id":"t1","name":"ipython","arguments":{"code":"edit()"}}]}}),
                serde_json::json!({"type":"message","id":"e","parentId":"d","message":{"role":"toolResult","toolCallId":"t1","toolName":"ipython","isError":false,"content":[{"type":"text","text":"huge output"}],"details":{"diffs":[{"path":"/work/src/main.rs"}]}}}),
                serde_json::json!({"type":"message","id":"f","parentId":"e","timestamp":"2026-11-22T15:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}),
            ],
        );
        let source = parsed(SourceProvider::Prime, &path);
        assert_eq!(source.id, "p1");
        assert_eq!(source.existing_summary.as_deref(), Some("## Goal\nfix"));
        assert_eq!(source.summary_turn_index, 1);
        assert_eq!(source.files_modified, set(&["src/main.rs"]));
        assert_eq!(source.files_read, set(&["src/lib.rs"]));
        assert_eq!(source.turns.len(), 2);
        assert_eq!(source.turns[0].user, "fix the bug");
        assert_eq!(source.turns[0].assistant, "");
        assert_eq!(source.turns[1].assistant, "done");
        let all = format!("{source:?}");
        assert!(
            !all.contains("secret") && !all.contains("abandoned") && !all.contains("huge output")
        );
        assert_eq!(source.ended_ms - source.started_ms, 3_600_000);
    }

    #[test]
    fn claude_extracts_file_ops_commands_branch_and_compact_summary() {
        let dir = tempfile::tempdir().unwrap();
        let row = |id: &str, parent: Option<&str>, kind: &str, content: Value| serde_json::json!({"type":kind,"uuid":id,"parentUuid":parent,"sessionId":"c1","cwd":"/app","gitBranch":"feat/auth","timestamp":"2026-11-22T14:02:00Z","message":{"role":kind,"content":content}});
        let mut summary = row("s", Some("z"), "user", serde_json::json!("prior summary"));
        summary["isCompactSummary"] = Value::Bool(true);
        let path = jsonl(
            dir.path(),
            "c.jsonl",
            &[
                row(
                    "a",
                    None,
                    "user",
                    serde_json::json!("<system-reminder>noise</system-reminder>Fix auth"),
                ),
                row(
                    "b",
                    Some("a"),
                    "assistant",
                    serde_json::json!([
                        {"type":"thinking","thinking":"hmm"},
                        {"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/app/src/session.rs"}},
                        {"type":"tool_use","id":"r2","name":"Read","input":{"file_path":"/app/src/session.rs"}},
                        {"type":"tool_use","id":"e1","name":"Edit","input":{"file_path":"/app/src/auth.rs","old_string":"a","new_string":"b"}},
                        {"type":"tool_use","id":"x1","name":"Bash","input":{"command":"cargo  test -p auth"}},
                    ]),
                ),
                row(
                    "c",
                    Some("b"),
                    "user",
                    serde_json::json!([{"type":"tool_result","tool_use_id":"x1","is_error":true,"content":"error: test failed\nExit code: 101"}]),
                ),
                row(
                    "d",
                    Some("c"),
                    "assistant",
                    serde_json::json!([{"type":"text","text":"I see the failure"}]),
                ),
                serde_json::json!({"type":"system","subtype":"compact_boundary","uuid":"z","parentUuid":null,"logicalParentUuid":"d"}),
                summary,
                row(
                    "e",
                    Some("s"),
                    "user",
                    serde_json::json!("<command-name>/compact</command-name>"),
                ),
                row("f", Some("e"), "user", serde_json::json!("now ship it")),
                row(
                    "g",
                    Some("f"),
                    "assistant",
                    serde_json::json!([{"type":"text","text":"Shipped."}]),
                ),
            ],
        );
        let source = parsed(SourceProvider::Claude, &path);
        assert_eq!(source.git_branch.as_deref(), Some("feat/auth"));
        assert_eq!(source.files_modified, set(&["src/auth.rs"]));
        assert_eq!(source.files_read, set(&["src/session.rs"]));
        assert_eq!(source.commands.len(), 1);
        assert_eq!(source.commands[0].cmd, "cargo test -p auth");
        assert_eq!(source.commands[0].exit, Some(101));
        assert_eq!(source.turns[0].user, "Fix auth");
        assert_eq!(
            source.turns[0]
                .tools
                .iter()
                .filter(|t| t.starts_with("→ read"))
                .count(),
            1
        );
        assert!(source.turns[0].errors[0].contains("test failed"));
        assert_eq!(source.existing_summary.as_deref(), Some("prior summary"));
        assert_eq!(source.turns.last().unwrap().assistant, "Shipped.");
        assert!(!format!("{source:?}").contains("hmm"));
    }

    #[test]
    fn codex_reads_patches_shell_and_survives_a_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let msg = |role: &str, text: &str| {
            let kind = if role == "user" {
                "input_text"
            } else {
                "output_text"
            };
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":role,"content":[{"type":kind,"text":text}]}})
        };
        let path = jsonl(
            dir.path(),
            "rollout.jsonl",
            &[
                serde_json::json!({"type":"session_meta","payload":{"id":"x1","cwd":"/repo","git":{"branch":"main","commit_hash":"1b1b76e0aa"}}}),
                msg("user", "<environment_context>cwd</environment_context>"),
                msg("user", "add a flag"),
                serde_json::json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"k1","arguments":"{\"command\":[\"bash\",\"-lc\",\"sed -n '1,80p' src/cli.rs\"]}"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"k1","output":"{\"output\":\"fn main\",\"metadata\":{\"exit_code\":0}}"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"k2","input":"*** Begin Patch\n*** Update File: src/cli.rs\n@@\n*** End Patch"}}),
                msg("assistant", "Added the flag."),
                msg("user", "stale request"),
                msg("assistant", "stale answer"),
                serde_json::json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
                msg("user", "run tests"),
                msg("assistant", "Tests pass."),
            ],
        );
        let source = parsed(SourceProvider::Codex, &path);
        assert_eq!(source.id, "x1");
        assert_eq!(source.head_commit.as_deref(), Some("1b1b76e0aa"));
        assert_eq!(source.files_modified, set(&["src/cli.rs"]));
        assert!(source.files_read.is_empty());
        assert_eq!(source.commands[0].exit, Some(0));
        let users: Vec<&str> = source.turns.iter().map(|t| t.user.as_str()).collect();
        assert_eq!(users, ["add a flag", "run tests"]);
        assert!(source.warnings[0].contains("rolled back"));
    }

    #[test]
    fn cursor_strips_user_query_and_reads_tool_paths() {
        let dir = tempfile::tempdir().unwrap();
        let id = "0b5f9c1e-1111-4222-8333-944455556666";
        let path = jsonl(
            dir.path(),
            &format!("projects/x/agent-transcripts/{id}.jsonl"),
            &[
                serde_json::json!({"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nrename foo\n</user_query>"}]}}),
                serde_json::json!({"role":"assistant","message":{"content":[{"type":"tool_use","name":"StrReplace","input":{"path":"/w/a.rs"}},{"type":"tool_use","name":"Read","input":{"path":"/w/b.rs"}},{"type":"text","text":"Renamed."}]}}),
            ],
        );
        let source = parsed(SourceProvider::Cursor, &path);
        assert_eq!(source.id, id);
        assert_eq!(source.turns[0].user, "rename foo");
        assert_eq!(source.turns[0].assistant, "Renamed.");
        assert_eq!(source.files_modified, set(&["/w/a.rs"]));
        assert_eq!(source.files_read, set(&["/w/b.rs"]));
    }

    #[test]
    fn antigravity_reads_planner_text_and_tool_steps() {
        let dir = tempfile::tempdir().unwrap();
        let id = "7c1d2e3f-aaaa-4bbb-8ccc-0123456789ab";
        let path = jsonl(
            dir.path(),
            &format!("brain/{id}/.system_generated/logs/transcript.jsonl"),
            &[
                serde_json::json!({"type":"USER_INPUT","content":"<USER_REQUEST>\nHello\n</USER_REQUEST><ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"}),
                serde_json::json!({"type":"VIEW_FILE","metadata":{"AbsolutePath":"/g/readme.md"}}),
                serde_json::json!({"type":"CODE_ACTION","metadata":{"TargetFile":"/g/app.py"}}),
                serde_json::json!({"type":"PLANNER_RESPONSE","content":"Updated app.py"}),
            ],
        );
        let source = parsed(SourceProvider::Antigravity, &path);
        assert_eq!(source.id, id);
        assert_eq!(source.turns[0].user, "Hello");
        assert_eq!(source.turns[0].assistant, "Updated app.py");
        assert_eq!(source.files_modified, set(&["/g/app.py"]));
        assert_eq!(source.files_read, set(&["/g/readme.md"]));
    }

    #[test]
    fn pa_threads_read_their_messages_with_unknown_files() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let threads = dir.path().join("threads");
        std::fs::create_dir_all(&threads).unwrap();
        std::fs::write(
            threads.join(format!("{id}.json")),
            serde_json::json!({"id":id,"provider":"claude","cwd":"/w","messages":[["user","plan it"],["assistant","planned"]]}).to_string(),
        )
        .unwrap();
        let resolved = resolve(
            &SourceRef {
                provider: "pa".into(),
                location: id.clone(),
            },
            dir.path(),
        )
        .unwrap();
        let source = parse(&resolved).unwrap();
        assert_eq!(source.id, id);
        assert!(!source.files_known);
        assert_eq!(source.turns[0].assistant, "planned");
    }
}
