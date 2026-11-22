//! Local agent connections. Each CLI owns authentication, tools, and native conversation state.
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::Value;

const MAX_RECORD: usize = 16 * 1024 * 1024;
const MAX_HISTORY: u64 = 128 * 1024 * 1024;
type Messages = Vec<(String, String)>;

fn command_available(program: &Path) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    let versions = std::fs::read_dir(platform_dir.join("versions")).ok()?;
    let mut found: Vec<_> = versions
        .flatten()
        .map(|entry| entry.path().join(binary_name))
        .filter(|path| path.is_file())
        .collect();
    found.sort();
    found.pop()
}

/// Resolve the Antigravity CLI executable (`PA_ANTIGRAVITY_BIN`, `agy`, `~/.local/bin/agy`,
/// `antigravity`, or T3 Code's managed `antigravity` binary).
pub(crate) fn antigravity_bin() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("PA_ANTIGRAVITY_BIN") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if command_available(Path::new("agy")) {
        return Some(PathBuf::from("agy"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let local = home.join(".local/bin/agy");
        if local.is_file() && command_available(&local) {
            return Some(local);
        }
    }
    if command_available(Path::new("antigravity")) {
        return Some(PathBuf::from("antigravity"));
    }
    t3_antigravity_bin().filter(|path| command_available(path))
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

/// Normalize a `pa` thinking level for external CLIs (`claude`, `codex`, `antigravity`).
/// Returns `None` for `"off"` (no effort flag) or empty values.
pub(crate) fn provider_effort(provider: &str, thinking: Option<&str>) -> Option<&'static str> {
    let level = thinking?.trim();
    if level.is_empty() || level == "off" {
        return None;
    }
    match provider {
        "claude" => match level {
            "minimal" | "low" => Some("low"),
            "medium" => Some("medium"),
            "high" => Some("high"),
            "xhigh" => Some("xhigh"),
            "max" => Some("max"),
            _ => None,
        },
        "codex" => match level {
            "minimal" => Some("minimal"),
            "low" => Some("low"),
            "medium" => Some("medium"),
            "high" => Some("high"),
            "xhigh" | "max" => Some("xhigh"),
            _ => None,
        },
        "antigravity" => match level {
            "minimal" | "low" => Some("low"),
            "medium" => Some("medium"),
            "high" => Some("high"),
            "xhigh" => Some("xhigh"),
            "max" => Some("max"),
            _ => None,
        },
        _ => None,
    }
}

/// Parse `agy models` output into deduplicated `(base_id, label, efforts)` entries.
pub(crate) fn parse_antigravity_models(raw: &str) -> Vec<(String, String, Vec<String>)> {
    let cleaned = strip_ansi(raw);
    let mut order: Vec<String> = Vec::new();
    let mut labels: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut efforts: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

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
        let (base_id, effort) = if let Some((prefix, suffix)) = raw_id.rsplit_once('-') {
            if matches!(suffix, "low" | "medium" | "high" | "xhigh" | "max") {
                (prefix.to_owned(), Some(suffix.to_owned()))
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
            (id, label, levels)
        })
        .collect()
}

pub(crate) fn antigravity_models() -> Result<Vec<(String, String, Vec<String>)>> {
    let bin = antigravity_bin().context("Antigravity CLI (agy) is not installed")?;
    let output = Command::new(bin)
        .arg("models")
        .stdin(Stdio::null())
        .output()
        .context("Could not query Antigravity models")?;
    ensure!(
        output.status.success(),
        "Antigravity models failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(parse_antigravity_models(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub(crate) fn connections() -> String {
    ["prime", "claude", "codex", "antigravity"]
        .iter()
        .map(|&name| {
            let available = match name {
                "prime" => true,
                "antigravity" => antigravity_bin().is_some(),
                other => command_available(Path::new(other)),
            };
            format!(
                "{name}\t{}",
                if available {
                    "available"
                } else {
                    "unavailable"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn uuid(value: &str) -> Result<String> {
    Ok(uuid::Uuid::parse_str(value)
        .context("Provider returned an invalid session ID")?
        .to_string())
}

fn text(message: &Value) -> Option<(String, String)> {
    let role = message["role"].as_str()?;
    if !matches!(role, "user" | "assistant") {
        return None;
    }
    let content = &message["content"];
    let body = if let Some(s) = content.as_str() {
        s.to_owned()
    } else {
        content
            .as_array()?
            .iter()
            .filter(|v| {
                matches!(
                    v["type"].as_str(),
                    Some("text" | "input_text" | "output_text")
                )
            })
            .filter_map(|v| v["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    (!body.trim().is_empty()).then(|| (role.to_owned(), body))
}

/// Enforce the allocation bound before deserializing even a single JSONL record.
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
        if !line.iter().all(u8::is_ascii_whitespace) {
            visit(serde_json::from_slice(&line).context("Invalid provider JSON record")?)?;
        }
    }
    Ok(())
}

fn strip_user_request_tags(content: &str) -> &str {
    if let Some(start) = content.find("<USER_REQUEST>") {
        let after = &content[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after.find("</USER_REQUEST>") {
            return after[..end].trim();
        }
    }
    content.trim()
}

fn antigravity_id_from_path(path: &Path) -> Option<String> {
    for component in path.components().rev() {
        let text = component.as_os_str().to_str()?;
        let stem = text.strip_suffix(".jsonl").unwrap_or(text);
        if uuid::Uuid::parse_str(stem).is_ok() {
            return Some(stem.to_owned());
        }
    }
    None
}

/// The live process that hosts `session` (a pa chat's daemon), or None when no runtime holds it.
fn session_holder(session: &Path) -> Option<u32> {
    let home = std::env::var_os("PA_CAPTAIN_NATIVE_HOME")
        .map_or_else(crate::config::get_agent_dir, PathBuf::from);
    pa_daemon::lease::live_lease_owner(&home, session).map(|owner| owner.pid)
}

/// The session directory of the native home a Prime thread turn runs in.
fn prime_session_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PRIME_AGENT_SESSION_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("PA_CAPTAIN_NATIVE_HOME")
        .map_or_else(crate::config::get_agent_dir, PathBuf::from)
        .join("sessions")
}

/// Create an empty native session for a new Prime thread, so the thread has one session file
/// from its first turn and pa can open it as an ordinary chat.
pub(crate) fn new_prime_session(cwd: &Path) -> Result<PathBuf> {
    let dir = prime_session_dir();
    std::fs::create_dir_all(&dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    let path = dir.join(format!("{id}.jsonl"));
    let header = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": id,
        "timestamp": crate::util_time::now_iso8601(),
        "cwd": cwd.display().to_string(),
    });
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("Could not create {}", path.display()))?;
    writeln!(file, "{header}")?;
    file.sync_all()?;
    Ok(path)
}

/// The assistant reply to `prompt` written to `session` after byte `offset`: the last assistant
/// message that follows the prompt's own user message.
fn prime_reply_after(session: &Path, offset: u64, prompt: &str) -> Result<Option<String>> {
    let mut file = File::open(session)?;
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offset))?;
    let mut reply = None;
    let mut seen_prompt = false;
    records(file, |event| {
        if event["type"] != "message" {
            return Ok(());
        }
        if let Some((role, body)) = text(&event["message"]) {
            if role == "user" {
                seen_prompt = body.trim() == prompt.trim();
                if seen_prompt {
                    reply = None;
                }
            } else if seen_prompt {
                reply = Some(body);
            }
        }
        Ok(())
    })?;
    Ok(reply)
}

/// Run one Prime turn through the shared daemon (`--mode acp`). The daemon hosts one runtime per
/// session file, so a pa chat open on the same session shows this turn live, and a prompt sent
/// from either side queues behind the running turn instead of starting a second one.
fn prime_daemon_turn(
    mut cmd: Command,
    cwd: &Path,
    session: &Path,
    prompt: &str,
    hidden_context: Option<&str>,
) -> Result<String> {
    // The faux seam reads a script file in daemon mode; a test may pass the script inline.
    let faux = match std::env::var("PRIME_AGENT_FAUX_SCRIPT") {
        Ok(script) if !Path::new(&script).is_file() => {
            let file = std::env::temp_dir().join(format!("pa-faux-{}.json", uuid::Uuid::new_v4()));
            std::fs::write(&file, script)?;
            cmd.env("PRIME_AGENT_FAUX_SCRIPT", &file);
            Some(file)
        }
        _ => None,
    };
    let offset = std::fs::metadata(session)
        .with_context(|| format!("Missing native session {}", session.display()))?
        .len();
    let mut child = cmd
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Could not start prime")?;
    let mut stdin = child.stdin.take().context("Missing prime input")?;
    let mut lines = BufReader::new(child.stdout.take().context("Missing prime output")?).lines();
    let mut call = |id: u64, method: &str, params: Value| -> Result<Value> {
        let frame =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{frame}")?;
        stdin.flush()?;
        loop {
            let line = lines.next().context("Prime stopped before it answered")??;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["id"] != id {
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error["data"]["details"]
                    .as_str()
                    .or_else(|| error["message"].as_str())
                    .unwrap_or("Prime turn failed");
                bail!("{detail}");
            }
            return Ok(message["result"].clone());
        }
    };
    let outcome = (|| -> Result<()> {
        call(
            1,
            "initialize",
            serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )?;
        let created = call(
            2,
            "session/new",
            serde_json::json!({"cwd": cwd.display().to_string(), "mcpServers": []}),
        )?;
        let session_id = created["sessionId"]
            .as_str()
            .context("Prime did not return a session")?
            .to_owned();
        let mut request = serde_json::json!({"sessionId": session_id, "prompt": [{"type": "text", "text": prompt}]});
        if let Some(context) = hidden_context {
            // The open chat's system prompt belongs to the chat, so the context rides the
            // prompt as a hidden row the daemon writes before it.
            request["_meta"] =
                serde_json::json!({"ai.primeintellect.prime-agent": {"hiddenContext": context}});
        }
        let done = call(3, "session/prompt", request)?;
        ensure!(
            done["stopReason"] == "end_turn",
            "Prime turn ended with {}",
            done["stopReason"]
        );
        Ok(())
    })();
    // ACP mode exits when its input closes.
    drop(stdin);
    drop(lines);
    if outcome.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    if let Some(file) = faux {
        let _ = std::fs::remove_file(file);
    }
    outcome?;
    prime_reply_after(session, offset, prompt)?
        .context("Provider did not return an assistant reply")
}

/// One turn's input: the text the transcript shows as the user message, and context the
/// model reads with it that no transcript shows (the Captain's instructions and state).
pub(crate) struct TurnPrompt<'a> {
    pub text: &'a str,
    pub hidden_context: Option<&'a str>,
}

pub(crate) fn turn(
    parent: uuid::Uuid,
    provider: &str,
    native_id: Option<&str>,
    cwd: &Path,
    input: &TurnPrompt<'_>,
    thread_model: Option<&str>,
    thread_thinking: Option<&str>,
) -> Result<(String, String)> {
    // A provider CLI keeps its own transcript, which pa never renders: it gets the context
    // inline. A Prime session is rendered by pa, so the context travels out of band.
    let inline;
    let prompt = match (provider, input.hidden_context) {
        ("prime", _) | (_, None) => input.text,
        (_, Some(context)) => {
            inline = format!("{context}\n\n{}", input.text);
            inline.as_str()
        }
    };
    let mut cmd = match provider {
        "prime" => Command::new(std::env::current_exe()?),
        "codex" | "claude" => Command::new(provider),
        "antigravity" => {
            Command::new(antigravity_bin().context("Antigravity CLI (agy) is not installed")?)
        }
        _ => bail!("Unknown provider {provider}"),
    };
    let executable = std::env::current_exe()?;
    let config_dir = crate::config::get_agent_dir();
    let mut tool_env = std::collections::BTreeMap::from([(
        "PRIME_AGENT_CODING_AGENT_DIR",
        config_dir.to_string_lossy().into_owned(),
    )]);
    for key in ["PRIME_AGENT_DAEMON_SOCKET", "PRIME_AGENT_SESSION_DIR"] {
        if let Ok(value) = std::env::var(key) {
            tool_env.insert(key, value);
        }
    }
    let tool_args = [
        "thread".to_owned(),
        "tool-server".to_owned(),
        parent.to_string(),
    ];
    // A Captain lead runs with its project's model and thinking level; the variables never reach nested turns.
    let model = std::env::var("PA_THREAD_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .or_else(|| {
            thread_model
                .filter(|m| !m.trim().is_empty())
                .map(str::to_owned)
        });
    let thinking = std::env::var("PA_THREAD_THINKING")
        .ok()
        .filter(|level| !level.trim().is_empty())
        .or_else(|| {
            thread_thinking
                .filter(|t| !t.trim().is_empty())
                .map(str::to_owned)
        });
    cmd.env_remove("PA_THREAD_MODEL");
    cmd.env_remove("PA_THREAD_THINKING");
    let instructions="When the user asks to spin up a subthread, use prime_threads spawn_subthread immediately. Pass a task when work is assigned. Use read_subthread to inspect the result.";
    let uses_stdin = provider != "antigravity";
    match provider {
        "prime" => {
            // A Captain thread's durable manifest and native model credentials have distinct homes.
            if let Some(native_home) = std::env::var_os("PA_CAPTAIN_NATIVE_HOME") {
                cmd.env("PRIME_AGENT_CODING_AGENT_DIR", native_home);
                if let Some(socket) = std::env::var_os("PA_CAPTAIN_NATIVE_SOCKET") {
                    cmd.env("PRIME_AGENT_DAEMON_SOCKET", socket);
                }
            }
            let prefix = serde_json::json!([executable, "thread", "tool-call", parent.to_string()]);
            let hint=format!("If asked to spin up a subthread, call the installed thread tool from python_repl. Use subprocess.run({prefix} + ['spawn_subthread', json.dumps(arguments)], env={{**os.environ, 'PRIME_AGENT_CODING_AGENT_DIR': {}}}, check=True, capture_output=True,text=True). Import os, json, subprocess first. arguments is a dict with optional provider and task. Read a child with read_subthread and its id.",serde_json::to_string(&config_dir)?);
            if let Some(id) = native_id.filter(|id| Path::new(id).is_absolute()) {
                if session_holder(Path::new(id)).is_some() {
                    // An open pa chat hosts this session. Send the turn through its daemon so
                    // the chat shows it live and the two sides never run turns at once.
                    let mut daemon = Command::new(&executable);
                    for (key, value) in cmd.get_envs() {
                        match value {
                            Some(value) => daemon.env(key, value),
                            None => daemon.env_remove(key),
                        };
                    }
                    daemon
                        .args(["--mode", "acp", "--append-system-prompt", &hint, "--cwd"])
                        .arg(cwd);
                    if let Some(model) = &model {
                        daemon.arg("--model").arg(model);
                    }
                    if let Some(level) = &thinking {
                        daemon.arg("--thinking").arg(level);
                    }
                    daemon.arg("--resume").arg(id);
                    let reply = prime_daemon_turn(
                        daemon,
                        cwd,
                        Path::new(id),
                        prompt,
                        input.hidden_context,
                    )?;
                    return Ok((id.to_owned(), reply));
                }
            }
            // The faux seam takes an inline script in print mode; a daemon test passes a file.
            if let Ok(script) = std::env::var("PRIME_AGENT_FAUX_SCRIPT") {
                if Path::new(&script).is_file() {
                    cmd.env("PRIME_AGENT_FAUX_SCRIPT", std::fs::read_to_string(&script)?);
                }
            }
            cmd.args(["--mode", "json", "--append-system-prompt", &hint]);
            if let Some(context) = input.hidden_context {
                // Print mode builds the system prompt for this turn only; nothing persists it.
                cmd.arg("--append-system-prompt").arg(context);
            }
            cmd.arg("--cwd").arg(cwd);
            if let Some(model) = &model {
                cmd.arg("--model").arg(model);
            }
            if let Some(level) = &thinking {
                cmd.arg("--thinking").arg(level);
            }
            if let Some(id) = native_id {
                cmd.arg("--resume").arg(id);
            }
        }
        "claude" => {
            let config = serde_json::json!({"mcpServers":{"prime_threads":{"command":executable,"args":tool_args,"env":tool_env}}});
            cmd.args([
                "--print",
                "--output-format",
                "json",
                "--mcp-config",
                &config.to_string(),
                "--append-system-prompt",
                instructions,
                "--allowedTools",
                "mcp__prime_threads__spawn_subthread,mcp__prime_threads__read_subthread",
            ]);
            if let Some(model) = &model {
                cmd.arg("--model").arg(model);
            }
            if let Some(effort) = provider_effort("claude", thinking.as_deref()) {
                cmd.arg("--effort").arg(effort);
            }
            if let Some(id) = native_id {
                cmd.arg("--resume").arg(uuid(id)?);
            }
        }
        "codex" => {
            // JSON strings/arrays here are also valid TOML values. No shell evaluates them.
            cmd.arg("-c")
                .arg(format!(
                    "mcp_servers.prime_threads.command={}",
                    serde_json::to_string(&executable)?
                ))
                .arg("-c")
                .arg(format!(
                    "mcp_servers.prime_threads.args={}",
                    serde_json::to_string(&tool_args)?
                ));
            for (key, value) in &tool_env {
                cmd.arg("-c").arg(format!(
                    "mcp_servers.prime_threads.env.{key}={}",
                    serde_json::to_string(value)?
                ));
            }
            if let Some(model) = &model {
                cmd.arg("-c")
                    .arg(format!("model={}", serde_json::to_string(model)?));
            }
            if let Some(effort) = provider_effort("codex", thinking.as_deref()) {
                cmd.arg("-c").arg(format!(
                    "model_reasoning_effort={}",
                    serde_json::to_string(effort)?
                ));
            }
            cmd.arg("exec");
            if let Some(id) = native_id {
                cmd.args(["resume", "--json", "--skip-git-repo-check"])
                    .arg(uuid(id)?);
            } else {
                cmd.args(["--json", "--skip-git-repo-check"]);
            }
            cmd.arg("-");
        }
        "antigravity" => {
            // Print mode has no one to approve a tool call, so agy's safety check denies
            // commands such as `pa thread captain act` and the turn ends without a reply.
            // The other adapters and pa's own antigravity provider run unattended too.
            cmd.args([
                "--output-format",
                "stream-json",
                "--dangerously-skip-permissions",
            ]);
            if let Some(model) = &model {
                cmd.arg("--model").arg(model);
            }
            if let Some(effort) = provider_effort("antigravity", thinking.as_deref()) {
                cmd.arg("--effort").arg(effort);
            }
            if let Some(id) = native_id {
                cmd.arg("--conversation").arg(uuid(id)?);
            }
            // Attach the prompt directly to `-p=` so leading dashes in user text are never parsed as flags.
            let full_prompt = if native_id.is_none() {
                format!(
                    "[Thread tool note: if asked to spin up a subthread, run `{exe} thread tool-call {parent} spawn_subthread '<json>'` with `PRIME_AGENT_CODING_AGENT_DIR={dir}`; read a child with `read_subthread`.]\n\n{prompt}",
                    exe = executable.display(),
                    dir = config_dir.display(),
                )
            } else {
                prompt.to_owned()
            };
            cmd.arg(format!("-p={full_prompt}"));
        }
        _ => unreachable!(),
    }
    let mut child = cmd
        .current_dir(cwd)
        .stdin(if uses_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("Could not start {provider}"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("Missing provider output")?;
    let mut id = None;
    let mut result = None;
    let mut streamed = String::new();
    let output = std::thread::scope(|scope| -> Result<()> {
        let input = stdin.map(|mut pipe| scope.spawn(move || pipe.write_all(prompt.as_bytes())));
        let parsed = records(stdout, |event| {
            match (provider, event["type"].as_str()) {
                ("claude", Some("result")) => {
                    ensure!(event["is_error"] != true, "Claude returned a failed turn");
                    id = event["session_id"].as_str().map(uuid).transpose()?;
                    result = event["result"].as_str().map(str::to_owned);
                }
                ("codex", Some("thread.started")) => {
                    id = event["thread_id"].as_str().map(uuid).transpose()?;
                }
                ("codex", Some("item.completed")) if event["item"]["type"] == "agent_message" => {
                    result = event["item"]["text"].as_str().map(str::to_owned);
                }
                ("codex", Some("turn.failed")) => bail!("Codex reported a failed turn"),
                ("prime", Some("session")) => id = event["id"].as_str().map(str::to_owned),
                ("prime", Some("message_end")) if event["message"]["role"] == "assistant" => {
                    ensure!(
                        !matches!(
                            event["message"]["stopReason"].as_str(),
                            Some("error" | "aborted")
                        ),
                        "Prime turn failed"
                    );
                    if let Some((_, body)) = text(&event["message"]) {
                        result = Some(body);
                    }
                }
                ("antigravity", _) => {
                    // stream-json: `step_update` events carry the reply as `agent_response`
                    // text deltas; the final `result` may repeat it in `response` or leave
                    // it empty. A single JSON object without `event` is the older shape.
                    if event["event"].as_str() == Some("step_update") {
                        let update = &event["step_update"];
                        if update["step_type"].as_str() == Some("agent_response") {
                            if let Some(delta) = update["text_delta"].as_str() {
                                streamed.push_str(delta);
                            }
                        }
                        return Ok(());
                    }
                    let payload = if event["event"].as_str() == Some("result") {
                        &event["result"]
                    } else if event.get("conversation_id").is_some()
                        && event.get("status").is_some()
                    {
                        &event
                    } else {
                        return Ok(());
                    };
                    if let Some(status) = payload["status"].as_str() {
                        ensure!(
                            status.eq_ignore_ascii_case("SUCCESS"),
                            "{}",
                            payload["error"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .unwrap_or("Antigravity returned a failed turn")
                        );
                    }
                    if let Some(conv_id) = payload["conversation_id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                    {
                        id = Some(uuid(conv_id)?);
                    }
                    let body = payload["response"]
                        .as_str()
                        .filter(|body| !body.trim().is_empty())
                        .unwrap_or(&streamed);
                    result = Some(body.trim_end_matches('\n').to_owned());
                }
                _ => {}
            }
            Ok(())
        });
        if parsed.is_err() {
            let _ = child.kill();
        }
        let status = child.wait()?;
        parsed?;
        ensure!(status.success(), "{provider} exited with {status}");
        if let Some(input) = input {
            input
                .join()
                .map_err(|_| anyhow!("Provider input thread stopped"))??;
        }
        Ok(())
    });
    output?;
    let id = id.context("Provider did not return a session ID")?;
    if let Some(expected) = native_id {
        if provider != "prime" || !Path::new(expected).is_absolute() {
            ensure!(id == expected, "Provider resumed a different session");
        }
    }
    Ok((
        native_id.unwrap_or(&id).to_owned(),
        result.context("Provider did not return an assistant reply")?,
    ))
}

pub(crate) fn import(provider: &str, path: &Path) -> Result<(String, Messages, PathBuf)> {
    ensure!(
        matches!(provider, "prime" | "claude" | "codex" | "antigravity"),
        "Unknown provider {provider}"
    );
    ensure!(
        path.metadata()?.is_file(),
        "Select a regular conversation file"
    );
    let file = File::open(path).context("Could not open selected conversation")?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_HISTORY,
        "Select a regular conversation file of at most 128 MiB"
    );
    let mut id = if provider == "antigravity" {
        antigravity_id_from_path(path)
    } else {
        None
    };
    let mut rows = Vec::new();
    let mut messages = Vec::new();
    let mut cwd = None;
    let mut entry_ids = HashSet::new();
    records(file.take(meta.len()), |event| {
        let kind = event["type"].as_str();
        let candidate = match (provider, kind) {
            ("prime", Some("session")) => event["id"].as_str(),
            ("codex", Some("session_meta")) => event["payload"]["id"]
                .as_str()
                .or_else(|| event["payload"]["session_id"].as_str()),
            ("claude", Some("user" | "assistant")) if event["isSidechain"] != true => {
                event["sessionId"].as_str()
            }
            ("antigravity", _) => event["conversation_id"].as_str(),
            _ => None,
        };
        if provider == "codex"
            && kind == Some("event_msg")
            && matches!(
                event["payload"]["type"].as_str(),
                Some("thread_rolled_back" | "thread_rollback" | "rollback")
            )
        {
            bail!("This Codex conversation contains a rollback; import a clean fork of its active branch");
        }
        if let Some(candidate) = candidate {
            let location = if provider == "codex" {
                &event["payload"]["cwd"]
            } else {
                &event["cwd"]
            };
            if let Some(location) = location.as_str() {
                let location = PathBuf::from(location);
                ensure!(
                    location.is_absolute(),
                    "Conversation working directory must be absolute"
                );
                cwd = Some(location);
            }
            ensure!(
                id.as_deref().is_none_or(|known| known == candidate),
                "Conversation contains multiple session IDs"
            );
            id = Some(uuid(candidate)?);
        } else if provider == "antigravity" {
            if let Some(location) = event["cwd"].as_str() {
                let location = PathBuf::from(location);
                ensure!(
                    location.is_absolute(),
                    "Conversation working directory must be absolute"
                );
                cwd = Some(location);
            }
        }
        match provider {
            "codex" if kind == Some("response_item") && event["payload"]["type"] == "message" => {
                if let Some(msg) = text(&event["payload"]) {
                    messages.push(msg);
                }
            }
            "antigravity" => match kind {
                Some("USER_INPUT") => {
                    if let Some(body) = event["content"].as_str() {
                        let cleaned = strip_user_request_tags(body);
                        if !cleaned.is_empty() {
                            messages.push(("user".to_owned(), cleaned.to_owned()));
                        }
                    }
                }
                Some("PLANNER_RESPONSE") => {
                    if let Some(body) = event["content"].as_str() {
                        if !body.trim().is_empty() {
                            messages.push(("assistant".to_owned(), body.trim().to_owned()));
                        }
                    }
                }
                _ => {}
            },
            "prime" | "claude" => {
                if event["isSidechain"] == true {
                    return Ok(());
                }
                let (key, parent) = if provider == "prime" {
                    ("id", "parentId")
                } else {
                    ("uuid", "parentUuid")
                };
                if let Some(entry) = event[key].as_str().filter(|_| kind != Some("session")) {
                    ensure!(
                        entry_ids.insert(entry.to_owned()),
                        "Duplicate conversation entry ID"
                    );
                    let msg = if matches!(kind, Some("message" | "user" | "assistant"))
                        && event["isMeta"] != true
                    {
                        text(&event["message"])
                    } else {
                        None
                    };
                    rows.push((
                        entry.to_owned(),
                        event[parent].as_str().map(str::to_owned),
                        msg,
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    if let Some(last) = rows.last() {
        let mut active = Some(last.0.as_str());
        let mut seen = HashSet::new();
        for (key, parent, msg) in rows.iter().rev() {
            if active == Some(key.as_str()) {
                ensure!(seen.insert(key), "Cycle in conversation history");
                if let Some(msg) = msg {
                    messages.push(msg.clone());
                }
                active = parent.as_deref();
            }
        }
        ensure!(
            active.is_none(),
            "Cannot resolve the active conversation branch"
        );
        messages.reverse();
    }
    let id = id.context("Missing native session header or session ID")?;
    // A Prime session header alone is a valid continuation target: the next turn
    // resumes that session and becomes its first user message.
    ensure!(
        provider == "prime" || !messages.is_empty(),
        "Conversation has no visible user or assistant messages"
    );
    let selector = if provider == "prime" {
        path.canonicalize()?
            .to_str()
            .context("Session path is not UTF-8")?
            .to_owned()
    } else {
        id
    };
    let cwd = match (provider, cwd) {
        (_, Some(dir)) => dir,
        ("antigravity", None) => std::env::current_dir()?,
        (_, None) => bail!("Missing native working directory"),
    };
    Ok((selector, messages, cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skips_abandoned_branches_and_tool_payloads() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        for (key, parent, role, content) in [
            ("a", None, "user", serde_json::json!("question")),
            ("b", Some("a"), "assistant", serde_json::json!("abandoned")),
            (
                "c",
                Some("a"),
                "assistant",
                serde_json::json!([{"type":"text","text":"kept"},{"type":"tool_use","input":"hidden"}]),
            ),
        ] {
            writeln!(file,"{}",serde_json::json!({"type":role,"sessionId":id,"cwd":"/workspace","uuid":key,"parentUuid":parent,"message":{"role":role,"content":content}})).unwrap();
        }
        assert_eq!(
            import("claude", file.path()).unwrap(),
            (
                id,
                vec![
                    ("user".into(), "question".into()),
                    ("assistant".into(), "kept".into())
                ],
                PathBuf::from("/workspace")
            )
        );
    }

    #[test]
    fn a_daemon_turn_reads_the_reply_to_its_own_prompt() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let header = r#"{"type":"session","version":3,"id":"s","cwd":"/tmp"}"#;
        writeln!(file, "{header}").unwrap();
        let offset = file.as_file().metadata().unwrap().len();
        for (role, text) in [
            ("user", "earlier"),
            ("assistant", "old reply"),
            ("user", "status?"),
            ("assistant", "working on it"),
            ("assistant", "all done"),
            ("user", "queued after"),
        ] {
            let row =
                serde_json::json!({"type": "message", "message": {"role": role, "content": text}});
            writeln!(file, "{row}").unwrap();
        }
        assert_eq!(
            prime_reply_after(file.path(), offset, "status?")
                .unwrap()
                .as_deref(),
            Some("all done")
        );
        assert_eq!(
            prime_reply_after(file.path(), offset, "missing").unwrap(),
            None
        );
    }

    #[test]
    fn prime_import_accepts_a_session_with_only_its_header() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"session","id":id,"cwd":"/workspace"})
        )
        .unwrap();
        let (_, messages, cwd) = import("prime", file.path()).unwrap();
        assert!(messages.is_empty());
        assert_eq!(cwd, PathBuf::from("/workspace"));
        assert!(import("claude", file.path()).is_err());
    }

    #[test]
    fn prime_import_follows_the_persisted_leaf_instead_of_replaying_siblings() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let records = [
            serde_json::json!({"type":"session","id":id,"cwd":"/workspace"}),
            serde_json::json!({"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"task"}}),
            serde_json::json!({"type":"message","id":"b","parentId":"a","message":{"role":"assistant","content":"earlier answer"}}),
            serde_json::json!({"type":"message","id":"c","parentId":"a","message":{"role":"assistant","content":"new answer"}}),
        ];
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
        let (_, messages, _) = import("prime", file.path()).unwrap();
        assert_eq!(
            messages,
            vec![
                ("user".into(), "task".into()),
                ("assistant".into(), "new answer".into())
            ]
        );
    }

    #[test]
    fn codex_rollback_requires_the_active_transcript() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        for record in [
            serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":"/workspace"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"stale"}]}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
        ] {
            writeln!(file, "{record}").unwrap();
        }
        assert!(import("codex", file.path())
            .unwrap_err()
            .to_string()
            .contains("rollback"));
    }

    #[test]
    fn parses_antigravity_models_and_thinking_efforts() {
        let sample = "\r\x1b[32mFetching available models...\x1b[0m\r\
gemini-4-neon-high              Gemini 4 Neon (High)\n\
gemini-4-neon-xhigh             Gemini 4 Neon (XHigh)\n\
gemini-4-neon-low               Gemini 4 Neon (Low)\n\
gemini-pro-orchestrator-neon    Orchestrator (Neon)\n\
claude-opus-5-5-medium          Claude Opus 5.5 (Medium)\n";
        let parsed = parse_antigravity_models(sample);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].0, "gemini-4-neon");
        assert_eq!(parsed[0].1, "Gemini 4 Neon");
        assert_eq!(parsed[0].2, vec!["low", "high", "xhigh"]);
        assert_eq!(parsed[1].0, "gemini-pro-orchestrator-neon");
        assert_eq!(parsed[1].1, "Orchestrator (Neon)");
        assert!(parsed[1].2.is_empty());
        assert_eq!(parsed[2].0, "claude-opus-5-5");
        assert_eq!(parsed[2].1, "Claude Opus 5.5");
        assert_eq!(parsed[2].2, vec!["medium"]);

        assert_eq!(provider_effort("antigravity", Some("xhigh")), Some("xhigh"));
        assert_eq!(provider_effort("antigravity", Some("minimal")), Some("low"));
        assert_eq!(provider_effort("antigravity", Some("off")), None);
        assert_eq!(provider_effort("claude", Some("max")), Some("max"));
        assert_eq!(provider_effort("codex", Some("high")), Some("high"));
    }

    #[test]
    fn antigravity_import_extracts_messages_and_id_from_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let log_dir = temp
            .path()
            .join("brain")
            .join(&id)
            .join(".system_generated/logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let file = log_dir.join("transcript.jsonl");
        let records = [
            serde_json::json!({"step_index":0,"type":"USER_INPUT","content":"<USER_REQUEST>\nHello Antigravity\n</USER_REQUEST>\n<ADDITIONAL_METADATA>ignored</ADDITIONAL_METADATA>"}),
            serde_json::json!({"step_index":1,"type":"PLANNER_RESPONSE","content":"Hello back!"}),
        ];
        let body = records
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file, body + "\n").unwrap();
        let (selector, messages, _) = import("antigravity", &file).unwrap();
        assert_eq!(selector, id);
        assert_eq!(
            messages,
            vec![
                ("user".into(), "Hello Antigravity".into()),
                ("assistant".into(), "Hello back!".into()),
            ]
        );
    }
}
