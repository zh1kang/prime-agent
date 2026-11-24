//! User-facing commands for durable threads.
use anyhow::{bail, ensure, Context, Result};
use pa_core::thread::{check_provider, check_thinking, Store, SubthreadJob, Thread};
use std::io::{self, Read};
use std::path::Path;
use uuid::Uuid;

const HELP: &str =
    "pa thread history [search text]
pa thread absorb [--focus <text>] [--json] [--no-llm] <provider> <path|id> [<provider> <path|id>...]
pa thread captain [status|act <operation>|stop|resume|delete]
pa thread providers | models <antigravity|cursor> | connect <claude|codex|antigravity|cursor> | list | new <prime|claude|codex|antigravity|cursor> | import <provider> <native-log> [--context-only]
pa thread <id> send <message> | history | info | context | pin <note>
pa thread <id> switch <provider> | model [model|default] | thinking [level|default] | child [provider] | children | recover
pa thread <id> worktree list | move <existing-root> | create <path> <branch>
pa thread <id> review gui | status | diff | staged | stage <path>... | unstage <path>... | json
In pa, /review, /history, and /fsm edit open native pages.
Open threads in pa: F6 opens the Captain panel, and /resume lists saved chats.
Switching providers or worktrees opens a fresh native session on the next message.
Use context to preview the history sent with that message.";

pub(crate) fn run(args: &[String]) -> Result<()> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    if matches!(words.as_slice(), [] | ["help" | "--help" | "-h"]) {
        println!("{HELP}");
        return Ok(());
    }
    if let ["tool-server", id] = words.as_slice() {
        return super::thread_tools::serve(id.parse()?);
    }
    if let ["tool-call", id, name, args] = words.as_slice() {
        return super::thread_tools::invoke(id.parse()?, name, args);
    }
    if let ["ensure", id, provider] = words.as_slice() {
        return ensure_thread(id.parse()?, provider);
    }
    if let [id, "send-stdin", rest @ ..] = words.as_slice() {
        // `--context-file PATH`: context the model reads with the task that the transcript
        // does not show (the Captain's instructions and state).
        let context = match rest {
            [] => None,
            ["--context-file", path] => {
                Some(std::fs::read_to_string(path).context("Could not read the turn context")?)
            }
            _ => bail!("{HELP}"),
        };
        let mut task = String::new();
        io::stdin()
            .take(128 * 1024 + 1)
            .read_to_string(&mut task)
            .context("Could not read the Captain task")?;
        ensure!(!task.trim().is_empty(), "Captain task is empty");
        ensure!(task.len() <= 128 * 1024, "Captain task is too large");
        let store = Store::open(&crate::config::get_agent_dir())?;
        let thread_id: Uuid = id.parse()?;
        let _lock = store.lock(thread_id)?;
        let mut thread = store.load(thread_id)?;
        ensure!(
            thread.pending.is_none(),
            "Thread has an interrupted turn; recover it first"
        );
        return send(
            &store,
            &mut thread,
            task.trim().to_owned(),
            context.as_deref(),
        );
    }
    if words == ["machine-graph"] {
        let spec = serde_json::from_reader(std::io::stdin().lock())?;
        println!(
            "{}",
            pa_tui::factory_view::machine_graph(&spec).context("Invalid machine graph")?
        );
        return Ok(());
    }
    if let ["run-graph", width, rows] = words.as_slice() {
        let snapshot = serde_json::from_reader(std::io::stdin().lock())?;
        println!(
            "{}",
            pa_tui::factory_view::run_view_text(&snapshot, width.parse()?, rows.parse()?)
                .context("Invalid factory run snapshot")?
        );
        return Ok(());
    }
    if words == ["factory-launch"] {
        let status = std::process::Command::new(
            crate::factory_command::resolve_kernel_python().map_err(anyhow::Error::msg)?,
        )
        .args([
            "-I",
            "-c",
            concat!(
                include_str!("machine_catalog.py"),
                "\n",
                include_str!("factory_launch.py")
            ),
        ])
        .arg(std::env::current_exe()?)
        .status()?;
        ensure!(status.success(), "Factory picker exited with {status}");
        return Ok(());
    }
    if let ["factory-host", cell, session, options @ ..] = words.as_slice() {
        // The Captain job worker owns this process; see captain_jobs.py. `--model` and
        // `--thinking` set the host session, whose model the steps that name none inherit.
        let cell_path = Path::new(cell);
        let cell = std::fs::read_to_string(cell_path).context("Missing factory host cell")?;
        if let Some(socket) = std::env::var_os("PRIME_AGENT_DAEMON_SOCKET") {
            return crate::thread_runtime::factory_host_on_daemon(
                Path::new(&socket),
                cell_path,
                cell,
                Path::new(session),
                options,
            );
        }
        let mut args = vec![
            "-p".to_owned(),
            "--resume".to_owned(),
            (*session).to_owned(),
        ];
        let mut rest = options.iter();
        while let Some(flag) = rest.next() {
            ensure!(
                matches!(*flag, "--model" | "--thinking"),
                "Unknown factory host option {flag}"
            );
            let value = rest.next().context("Missing factory host option value")?;
            args.extend([(*flag).to_owned(), (*value).to_owned()]);
        }
        let code = crate::print_runtime::run_factory_host(&args, cell);
        ensure!(code == 0, "Factory host exited with {code}");
        return Ok(());
    }
    if let ["captain", rest @ ..] = words.as_slice() {
        return run_captain(rest);
    }
    if let ["fsm-edit", rest @ ..] = words.as_slice() {
        let status = std::process::Command::new(
            crate::factory_command::resolve_kernel_python().map_err(anyhow::Error::msg)?,
        )
        .args([
            "-I",
            "-c",
            concat!(
                include_str!("machine_catalog.py"),
                "\n",
                include_str!("machine_editor.py")
            ),
        ])
        .arg(std::env::current_exe()?)
        .args(rest)
        .status()?;
        ensure!(status.success(), "Machine editor exited with {status}");
        return Ok(());
    }
    if let ["read-transcript", provider, path] = words.as_slice() {
        let (_, messages, cwd) = super::thread_runtime::import(provider, Path::new(path))?;
        serde_json::to_writer(
            io::stdout().lock(),
            &serde_json::json!({"messages": messages, "cwd": cwd}),
        )?;
        return Ok(());
    }
    if let ["absorb", rest @ ..] = words.as_slice() {
        return absorb(rest);
    }
    if let ["history", query @ ..] = words.as_slice() {
        let status = std::process::Command::new("python3")
            .args(["-I", "-c", include_str!("thread_history.py")])
            .env("PA_HISTORY_EXECUTABLE", std::env::current_exe()?)
            .args(query)
            .status()
            .context("History requires python3")?;
        ensure!(status.success(), "History exited with {status}");
        return Ok(());
    }
    if let ["review", ..] = words.as_slice() {
        return super::thread_workspace::review(&std::env::current_dir()?, &args[1..]);
    }
    if words == ["providers"] {
        println!("{}", super::thread_runtime::connections());
        return Ok(());
    }
    if let ["models", provider] = words.as_slice() {
        match *provider {
            "antigravity" => {
                for (id, label, efforts) in super::thread_runtime::antigravity_models()? {
                    println!("{id}\t{label}\t{}", efforts.join(","));
                }
                return Ok(());
            }
            "cursor" => {
                for (id, label, efforts) in super::thread_runtime::cursor_models()? {
                    println!("{id}\t{label}\t{}", efforts.join(","));
                }
                return Ok(());
            }
            _ => bail!("Use `pa model list` for prime/claude/codex or `pa thread models <antigravity|cursor>`"),
        }
    }
    if let ["connect", provider] = words.as_slice() {
        // `codex login` replaces the stored auth file, so a second login while signed in could
        // sign the user out. Refuse before starting it.
        let status = match *provider {
            "claude" => Some(("claude", ["auth", "status"])),
            "codex" => Some(("codex", ["login", "status"])),
            _ => None,
        };
        if let Some((program, args)) = status {
            let state = sign_in_state(program, &args);
            if state == SignInState::Unclear {
                bail!(
                    "Could not tell whether {program} is signed in. Run `{program} {}` to check.",
                    args.join(" ")
                );
            }
            if state == SignInState::SignedIn {
                bail!(
                    "{program} is already signed in. Run `{program} {}` first to sign in again.",
                    if program == "codex" {
                        "logout"
                    } else {
                        "auth logout"
                    }
                );
            }
        }
        let mut login = match *provider {
            "claude" => {
                let mut command = std::process::Command::new("claude");
                command.args(["auth", "login"]);
                command
            }
            "codex" => {
                let mut command = std::process::Command::new("codex");
                command.arg("login");
                command
            }
            "antigravity" => {
                let bin = super::thread_runtime::antigravity_bin()
                    .context("Antigravity CLI (agy) is not installed")?;
                let mut command = std::process::Command::new(bin);
                command.args(["acp", "--login"]);
                command
            }
            "cursor" => {
                let bin = super::thread_runtime::cursor_bin()
                    .context("Cursor CLI (cursor-agent) is not installed")?;
                let mut command = std::process::Command::new(bin);
                command.arg("login");
                command
            }
            "prime" => bail!("Open radiata and run /login to connect a Prime model provider"),
            _ => bail!("Unknown provider {provider}"),
        };
        ensure!(login.status()?.success(), "{provider} login failed");
        return Ok(());
    }
    let store = Store::open(&crate::config::get_agent_dir())?;
    match words.as_slice() {
        ["new", provider] => created(&store, &Thread::new(provider, std::env::current_dir()?)?),
        ["import", provider, path] | ["import", provider, path, "--context-only"] => {
            println!(
                "{}",
                import_thread(&store, provider, path, words.len() == 4)?
            );
            Ok(())
        }
        ["list"] => list(&store, None),
        [_, "chat"] => bail!("Open threads in pa: press F6 for the Captain or use /resume."),
        [id, ..] => command(
            &store,
            id.parse()
                .context("Expected a thread UUID; see pa thread help")?,
            &args[1..],
        ),
        [] => unreachable!(),
    }
}

/// `pa thread absorb`: print the `[absorbed-context]` packet (or the outcome
/// as JSON) for scripts and CLI-provider threads. It never calls a model: a
/// digest a pa chat already distilled is reused from the cache, otherwise the
/// chat is absorbed verbatim, by its own compaction summary, or as the
/// offline card. `--no-llm` states that explicitly.
fn absorb(args: &[&str]) -> Result<()> {
    let mut focus = None;
    let mut json = false;
    let mut sources = Vec::new();
    let mut rest = args.iter();
    while let Some(&arg) = rest.next() {
        match arg {
            "--focus" => focus = Some((*rest.next().context("--focus needs a value")?).to_owned()),
            "--json" => json = true,
            "--no-llm" => {}
            provider => sources.push(pa_core::absorb::SourceRef {
                provider: provider.to_owned(),
                location: (*rest
                    .next()
                    .with_context(|| format!("Name the {provider} chat's path or id"))?)
                .to_owned(),
            }),
        }
    }
    ensure!(!sources.is_empty(), "{HELP}");
    let agent_dir = crate::config::get_agent_dir();
    let cwd = std::env::current_dir()?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let outcome = pa_core::absorb::absorb(
        &pa_core::absorb::AbsorbRequest {
            agent_dir: &agent_dir,
            cwd: &cwd,
            sources,
            focus,
            budget_tokens: pa_core::absorb::DEFAULT_BUDGET_TOKENS,
            now_ms,
        },
        None,
    )?;
    if json {
        serde_json::to_writer(io::stdout().lock(), &outcome)?;
        println!();
    } else {
        println!("{}", outcome.content);
    }
    Ok(())
}

fn ensure_thread(id: Uuid, provider: &str) -> Result<()> {
    check_provider(provider)?;
    let store = Store::open(&crate::config::get_agent_dir())?;
    let _lock = store.lock(id)?;
    if store.ids()?.contains(&id) {
        let thread = store.load(id)?;
        ensure!(
            thread.provider == provider,
            "Thread {id} already uses {}",
            thread.provider
        );
        ensure!(
            thread.cwd == std::env::current_dir()?,
            "Thread {id} belongs to another workspace"
        );
    } else {
        let mut thread = Thread::new(provider, std::env::current_dir()?)?;
        thread.id = id;
        if provider == "prime" {
            // A Prime thread is a pa chat from the start, so pa can open it before its first turn.
            thread.native = Some(
                super::thread_runtime::new_prime_session(&thread.cwd)?
                    .display()
                    .to_string(),
            );
        }
        store.save(&thread)?;
    }
    println!("{id}");
    Ok(())
}

fn run_captain(args: &[&str]) -> Result<()> {
    let python = std::env::var_os("PA_CAPTAIN_PYTHON")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("python3"));
    let mut command = std::process::Command::new(python);
    command
        .args([
            "-I",
            "-c",
            concat!(
                include_str!("captain_backend.py"),
                "\n",
                include_str!("machine_catalog.py"),
                "\n",
                include_str!("captain_snapshots.py"),
                "\n",
                include_str!("captain_runs.py"),
                "\n",
                include_str!("captain_wake.py"),
                "\n",
                include_str!("captain_schedules.py"),
                "\n",
                include_str!("captain_router.py"),
                "\n",
                include_str!("captain_jobs.py"),
                "\n",
                include_str!("captain_factories.py"),
                "\n",
                include_str!("captain_modules.py"),
                "\n",
                include_str!("captain_main.py")
            ),
        ])
        .arg(std::env::current_exe()?)
        .args(args);
    let status = command.status().context("Captain requires Python 3")?;
    ensure!(status.success(), "Captain exited with {status}");
    Ok(())
}

fn import_thread(store: &Store, provider: &str, path: &str, context_only: bool) -> Result<Uuid> {
    let path = Path::new(path).canonicalize()?;
    let (id, messages, cwd) = super::thread_runtime::import(provider, &path)?;
    let mut t = Thread::new(
        provider,
        if context_only {
            std::env::current_dir()?
        } else {
            cwd.canonicalize()
                .context("Original workspace unavailable; use --context-only")?
        },
    )?;
    let _import_lock = store.lock(Uuid::nil())?;
    if !context_only {
        for existing in store.ids()? {
            let other = store.load(existing)?;
            if other.provider == *provider && other.native.as_ref() == Some(&id) {
                return Ok(existing);
            }
        }
        t.native = Some(id);
    }
    t.messages = messages;
    t.origin = Some(path.display().to_string());
    store.save(&t)?;
    Ok(t.id)
}

fn created(store: &Store, t: &Thread) -> Result<()> {
    store.save(t)?;
    println!("{}", t.id);
    Ok(())
}
fn list(store: &Store, parent: Option<Uuid>) -> Result<()> {
    for id in store.ids()? {
        let t = store.load(id)?;
        if parent.is_none() || t.parent == parent {
            println!(
                "{}\t{}\t{}\t{}",
                t.id,
                t.provider,
                t.cwd.display(),
                t.note.lines().next().unwrap_or("")
            );
        }
    }
    Ok(())
}

fn command(store: &Store, id: Uuid, args: &[String]) -> Result<()> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    if let ["subthread", task @ ..] = words.as_slice() {
        let arguments = if task.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::json!({"task": task.join(" ")})
        };
        return super::thread_tools::invoke(id, "spawn_subthread", &arguments.to_string());
    }
    let _lock = store.lock(id)?;
    let mut t = store.load(id)?;
    match words.as_slice() {
        ["history"] => {
            for (i, (role, text)) in t.messages.iter().enumerate() {
                println!("[{}] {role}: {text}", i + 1);
            }
            return Ok(());
        }
        ["context"] => { print!("{}", t.context()); return Ok(()); }
        ["children"] => return list(store, Some(id)),
        ["info"] => {
            println!("thread: {id}\nprovider: {}\nmodel: {:?}\nthinking: {:?}\nworkspace: {}\nparent: {:?}\nnative session: {:?}\nsource: {:?}\njob: {:?}\nunfinished turn: {:?}", t.provider, t.model, t.thinking, t.cwd.display(), t.parent, t.native, t.origin, t.job, t.pending);
            return Ok(());
        }
        ["run-queued"] => {
            ensure!(t.job == Some(SubthreadJob::Queued), "Child has no queued task");
            let msg=t.pending.clone().context("Missing task")?;
            t.job=Some(SubthreadJob::Running);
            if let Err(err)=send(store,&mut t,msg,None){
                t.job=Some(SubthreadJob::Failed(err.to_string())); store.save(&t)?; return Err(err);
            }
            return Ok(());
        }
        ["abandon"] => {
            // The user stopped this turn. Its provider session and checkout keep any partial
            // work, and the next message continues the same session.
            t.pending = None;
            t.job = None;
        }
        ["recover"] => {
            ensure!(t.pending.is_some(), "No interrupted turn");
            t.pending = None;
            t.job = None;
            // A Prime session file stays the chat pa opens; the daemon continues it.
            let keeps_session = t.provider == "prime"
                && t.native.as_deref().is_some_and(|n| Path::new(n).is_absolute() && Path::new(n).is_file());
            if !keeps_session {
                t.native = None;
            }
        }
        ["review", ..] => return super::thread_workspace::review(&t.cwd, &args[1..]),
        _ if t.pending.is_some() => bail!("An interrupted turn may have performed work. Inspect info and review status before running recover. Recovery starts a fresh native session."),
        ["worktree", ..] => {
            if let Some(cwd) = super::thread_workspace::worktree(&t.cwd, &args[1..])? {
                if cwd != t.cwd { t.cwd = cwd; t.native = None; }
            }
        }
        ["pin", note @ ..] if !note.is_empty() => t.note = note.join(" "),
        ["switch", provider] => {
            check_provider(provider)?;
            if t.provider != *provider {
                t.provider = (*provider).into();
                t.model = None;
                t.native = None;
            }
        }
        ["model"] => {
            println!("{}", t.model.as_deref().unwrap_or("default"));
            return Ok(());
        }
        ["model", value] => {
            let trimmed = value.trim();
            t.model = if trimmed.is_empty() || trimmed == "default" {
                None
            } else {
                Some(trimmed.to_owned())
            };
        }
        ["thinking" | "effort"] => {
            println!("{}", t.thinking.as_deref().unwrap_or("default"));
            return Ok(());
        }
        ["thinking" | "effort", value] => {
            let trimmed = value.trim().to_ascii_lowercase();
            if trimmed.is_empty() || trimmed == "default" {
                t.thinking = None;
            } else {
                check_thinking(&trimmed)?;
                t.thinking = Some(trimmed);
            }
        }
        ["child"] | ["child", _] => {
            let child = super::thread_tools::child(&t,words.get(1).copied())?;
            return created(store, &child);
        }
        ["send", parts @ ..] if !parts.is_empty() => {
            let msg = parts.join(" ");
            ensure!(!msg.trim().is_empty(), "Empty message");
            send(store, &mut t, msg, None)?;
            return Ok(());
        }
        _ => bail!("{HELP}"),
    }
    store.save(&t)
}

fn send(store: &Store, t: &mut Thread, msg: String, hidden_context: Option<&str>) -> Result<()> {
    let prompt = if t.native.is_none() && (!t.messages.is_empty() || !t.note.is_empty()) {
        format!("{}\nCurrent user request:\n{msg}", t.context())
    } else {
        msg.clone()
    };
    t.pending = Some(msg.clone());
    store.save(t)?;
    let turn = super::thread_runtime::turn(
        t.id,
        &t.provider,
        t.native.as_deref(),
        &t.cwd,
        &super::thread_runtime::TurnPrompt {
            text: &prompt,
            hidden_context: hidden_context.filter(|context| !context.trim().is_empty()),
        },
        t.model.as_deref(),
        t.thinking.as_deref(),
    );
    let (native, reply) = match turn {
        Ok(done) => done,
        Err(error) => {
            // The daemon ended the turn (stopped, failed, or no reply): nothing still runs.
            if error
                .downcast_ref::<super::thread_runtime::TurnSettled>()
                .is_some()
            {
                t.pending = None;
                store.save(t)?;
            }
            return Err(error);
        }
    };
    t.native = Some(native);
    t.pending = None;
    t.messages.push(("user".into(), msg));
    t.messages.push(("assistant".into(), reply.clone()));
    if t.job == Some(SubthreadJob::Running) {
        t.job = Some(SubthreadJob::Completed);
    }
    store.save(t)?;
    println!("{reply}");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignInState {
    SignedIn,
    SignedOut,
    /// The status command ran but its answer was not recognized. A login is refused then,
    /// because `codex login` deletes the existing credential file before it starts.
    Unclear,
}

/// What a provider CLI's status command reports. A missing program counts as signed out,
/// because the login cannot start either and reports that itself.
fn sign_in_state(program: &str, args: &[&str]) -> SignInState {
    let Ok(output) = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return SignInState::SignedOut;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_lowercase();
    if text.contains("not logged in")
        || text.contains("\"loggedin\": false")
        || text.contains("\"loggedin\":false")
    {
        return SignInState::SignedOut;
    }
    if output.status.success()
        && (text.contains("logged in")
            || text.contains("\"loggedin\": true")
            || text.contains("\"loggedin\":true"))
    {
        return SignInState::SignedIn;
    }
    SignInState::Unclear
}

#[cfg(all(test, unix))]
mod connect_tests {
    use std::os::unix::fs::PermissionsExt;

    fn fake(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_signed_in_provider_is_detected_from_its_status_command() {
        let dir = tempfile::tempdir().unwrap();
        let codex_in = fake(dir.path(), "codex-in", "echo 'Logged in using ChatGPT'");
        let codex_out = fake(dir.path(), "codex-out", "echo 'Not logged in'; exit 1");
        let claude_in = fake(dir.path(), "claude-in", "echo '{\"loggedIn\": true}'");
        let claude_out = fake(dir.path(), "claude-out", "echo '{\"loggedIn\": false}'");
        use super::SignInState::{SignedIn, SignedOut, Unclear};
        let codex_odd = fake(dir.path(), "codex-odd", "echo 'session expired'; exit 2");
        assert_eq!(
            super::sign_in_state(&codex_in, &["login", "status"]),
            SignedIn
        );
        assert_eq!(
            super::sign_in_state(&codex_out, &["login", "status"]),
            SignedOut
        );
        assert_eq!(
            super::sign_in_state(&claude_in, &["auth", "status"]),
            SignedIn
        );
        assert_eq!(
            super::sign_in_state(&claude_out, &["auth", "status"]),
            SignedOut
        );
        assert_eq!(
            super::sign_in_state(&codex_odd, &["login", "status"]),
            Unclear
        );
        assert_eq!(
            super::sign_in_state("/nonexistent/codex", &["login", "status"]),
            SignedOut
        );
    }
}
