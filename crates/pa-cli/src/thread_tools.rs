//! Per-turn thread tools. The provider receives a server bound to one parent ID.
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Context, Result};
use pa_core::thread::{Store, SubthreadJob, Thread};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

const SPAWN: &str = "spawn_subthread";
const READ: &str = "read_subthread";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spawn {
    provider: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    task: Option<String>,
}

pub(super) fn child(parent: &Thread, provider: Option<&str>) -> Result<Thread> {
    let target_provider = provider.unwrap_or(&parent.provider);
    let mut t = Thread::new(target_provider, parent.cwd.clone())?;
    t.parent = Some(parent.id);
    if target_provider == parent.provider {
        t.model.clone_from(&parent.model);
    }
    t.thinking.clone_from(&parent.thinking);
    t.note.clone_from(&parent.note);
    t.messages.clone_from(&parent.messages);
    if let Some(request) = &parent.pending {
        t.messages.push(("user".into(), request.clone()));
    }
    Ok(t)
}

fn spawn(store: &Store, parent: &Thread, args: Value) -> Result<Value> {
    let args: Spawn = serde_json::from_value(args)?;
    if let Some(task) = &args.task {
        ensure!(
            !task.trim().is_empty() && task.len() <= 16000,
            "Use 1 to 16000 bytes of task text"
        );
    }
    let mut t = child(parent, args.provider.as_deref())?;
    if let Some(model) = args
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        t.model = if model == "default" {
            None
        } else {
            Some(model.to_owned())
        };
    }
    if let Some(thinking) = args
        .thinking
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let normalized = thinking.to_ascii_lowercase();
        if normalized == "default" {
            t.thinking = None;
        } else {
            pa_core::thread::check_thinking(&normalized)?;
            t.thinking = Some(normalized);
        }
    }
    if let Some(task) = args.task {
        t.pending = Some(task);
        t.job = Some(SubthreadJob::Queued);
        store.save(&t)?;
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(["thread", &t.id.to_string(), "run-queued"])
            .current_dir(&t.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        pa_core::platform::process::set_new_session(&mut cmd);
        #[cfg(not(unix))]
        pa_core::platform::set_new_process_group(&mut cmd);
        let worker = match cmd.spawn() {
            Ok(worker) => worker,
            Err(err) => {
                t.job = Some(SubthreadJob::Failed(err.to_string()));
                store.save(&t)?;
                bail!("Child {} could not start: {err}", t.id);
            }
        };
        std::thread::spawn(move || {
            let mut worker = worker;
            let _ = worker.wait();
        });
        return Ok(json!({"id":t.id,"state":"launched"}));
    }
    store.save(&t)?;
    Ok(json!({"id":t.id,"state":"created"}))
}

fn call(store: &Store, parent: Uuid, name: &str, args: Value) -> Result<Value> {
    match name {
        SPAWN => spawn(store, &store.load(parent)?, args),
        READ => {
            let id = args["id"].as_str().context("Expected child ID")?.parse()?;
            let t = store.load(id)?;
            ensure!(
                t.parent == Some(parent),
                "Expected a direct child of this thread"
            );
            Ok(json!({"id":id,"pending":t.pending.is_some(),"job":t.job,"context":t.context()}))
        }
        _ => bail!("Unknown thread tool"),
    }
}

fn tools() -> Value {
    json!({"tools":[
        {"name":SPAWN,"description":"Create a real child conversation when the user asks to spin up a subthread. Supply task to dispatch work immediately. Returns the child ID.","inputSchema":{"type":"object","properties":{"provider":{"type":"string","enum":["prime","claude","codex","antigravity"]},"model":{"type":"string","minLength":1,"maxLength":160},"thinking":{"type":"string","enum":["off","minimal","low","medium","high","xhigh","max","default"]},"task":{"type":"string","minLength":1,"maxLength":16000}},"additionalProperties":false}},
        {"name":READ,"description":"Read bounded context and pending state of a direct child.","inputSchema":{"type":"object","required":["id"],"properties":{"id":{"type":"string"}},"additionalProperties":false}}
    ]})
}

pub(super) fn serve(parent: Uuid) -> Result<()> {
    use std::io::Read;
    let store = Store::open(&crate::config::get_agent_dir())?;
    store.load(parent)?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut line = String::new();
    loop {
        line.clear();
        if input.by_ref().take(65537).read_line(&mut line)? == 0 {
            break;
        }
        ensure!(line.len() <= 65536, "Thread tool request too large");
        let req: Value = serde_json::from_str(&line)?;
        let Some(id) = req.get("id") else { continue };
        let result = match req["method"].as_str() {
            Some("initialize") => {
                json!({"protocolVersion":match req["params"]["protocolVersion"].as_str(){Some("2024-11-05")=>"2024-11-05",Some("2025-03-26")=>"2025-03-26",_=>"2025-06-18"},"capabilities":{"tools":{}},"serverInfo":{"name":"prime_threads","version":"1.0"}})
            }
            Some("ping") => json!({}),
            Some("tools/list") => tools(),
            Some("tools/call") => {
                let params = &req["params"];
                match call(
                    &store,
                    parent,
                    params["name"].as_str().unwrap_or(""),
                    params["arguments"].clone(),
                ) {
                    Ok(v) => json!({"content":[{"type":"text","text":v.to_string()}]}),
                    Err(e) => {
                        json!({"isError":true,"content":[{"type":"text","text":format!("{e:#}")}]})
                    }
                }
            }
            _ => {
                writeln!(
                    output,
                    "{}",
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
                )?;
                output.flush()?;
                continue;
            }
        };
        writeln!(
            output,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"result":result})
        )?;
        output.flush()?;
    }
    Ok(())
}

pub(super) fn invoke(parent: Uuid, name: &str, args: &str) -> Result<()> {
    let store = Store::open(&crate::config::get_agent_dir())?;
    println!(
        "{}",
        call(&store, parent, name, serde_json::from_str(args)?)?
    );
    Ok(())
}
