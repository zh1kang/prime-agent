//! Exercise a native import, provider handoff, subsequent resume, and child isolation.
#![cfg(unix)]
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

#[test]
fn handoff_and_child_preserve_native_session_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let script = root.join("claude");
    fs::write(&script, r#"#!/bin/sh
set -eu
cat > "$CAPTURE/input"
echo "$@" > "$CAPTURE/args"
echo '{"type":"result","session_id":"cb16047f-8505-499e-9653-1a4825cbfc8c","result":"fixture reply","is_error":false}'
"#).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let invoke = |args: &[&str], input: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
            .args(["thread"])
            .args(args)
            .current_dir(root)
            .env("PRIME_AGENT_CODING_AGENT_DIR", root.join("config"))
            .env("CAPTURE", root)
            .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    };
    let log = root.join("external.jsonl");
    let hdr = serde_json::json!({"type":"session_meta","payload":{"id":"70e6c63b-a123-449d-b203-9ef9e6d05739","cwd":root}});
    let msg = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"original goal"}]}});
    fs::write(&log, format!("{hdr}\n{msg}\n")).unwrap();
    let id = invoke(&["import", "codex", log.to_str().unwrap()], "");
    assert_eq!(id, invoke(&["import", "codex", log.to_str().unwrap()], ""));
    // The same steps the removed plain chat loop took, as direct commands.
    invoke(&[&id, "pin", "preserve the old API"], "");
    invoke(&[&id, "switch", "claude"], "");
    invoke(&[&id, "send", "continue"], "");
    let context = fs::read_to_string(root.join("input")).unwrap();
    assert!(context.contains("original goal") && context.contains("preserve the old API"));
    invoke(&[&id, "send", "follow up"], "");
    assert_eq!(fs::read_to_string(root.join("input")).unwrap(), "follow up");
    assert!(fs::read_to_string(root.join("args"))
        .unwrap()
        .contains("--resume cb16047f"));
    let child = invoke(&[&id, "child", "codex"], "");
    assert!(invoke(&[&id, "children"], "").contains(&child));
    assert!(invoke(&[&child, "info"], "").contains("native session: None"));
    assert!(invoke(&[&child, "context"], "").contains("fixture reply"));
}

#[test]
fn mcp_starts_background_child_and_limits_reads_to_direct_children() {
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let root = tmp.path();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
            .arg("thread")
            .args(args)
            .current_dir(root)
            .env("PRIME_AGENT_CODING_AGENT_DIR", root.join("state"))
            .env("PRIME_AGENT_DAEMON_SOCKET", root.join("daemon.sock"))
            .env(
                "PRIME_AGENT_FAUX_SCRIPT",
                r#"{"responses":["verified child response"]}"#,
            )
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8(out.stdout).unwrap().trim().to_owned(),
        )
    };
    let (_, parent) = run(&["new", "prime"]);
    let (ok, reply) = run(&[
        "tool-call",
        &parent,
        "spawn_subthread",
        r#"{"task":"Reply once."}"#,
    ]);
    assert!(ok);
    let launch: serde_json::Value = serde_json::from_str(&reply).unwrap();
    let arg = serde_json::json!({"id":launch["id"]}).to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let (ok, response) = run(&["tool-call", &parent, "read_subthread", &arg]);
        assert!(ok);
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        if v["job"] == "completed" {
            assert!(v["context"]
                .as_str()
                .unwrap()
                .contains("verified child response"));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "child result: {v}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let (_, other) = run(&["new", "prime"]);
    assert!(!run(&["tool-call", &other, "read_subthread", &arg]).0);
}

#[test]
fn antigravity_provider_handles_models_thinking_and_conversation_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let script = root.join("fake-agy");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if args == ["--version"]:
    print("1.5.14")
    sys.exit(0)
if args == ["models"]:
    print("gemini-4-neon-high\tGemini 4 Neon (High)")
    print("gemini-4-neon-low\tGemini 4 Neon (Low)")
    sys.exit(0)
pathlib.Path(os.environ["CAPTURE"]).write_text(json.dumps(args))
cid = "8b88c0dc-9360-4f7e-9c0c-22b969492f41"
assert args[:2] == ["--output-format", "stream-json"], args
emit = lambda event, **body: print(json.dumps({"event": event, **body}), flush=True)
emit("init", conversation_id=cid, init={"model": "gemini-4-neon"})
# Like agy print mode: without the skip flag the safety check denies the tool call
# and the turn ends with no reply text.
emit("step_update", step_update={"conversation_id": cid, "step_type": "tool_call", "status": "running"})
if "--dangerously-skip-permissions" in args:
    for delta in ["antigravity ", "reply\n"]:
        emit("step_update", step_update={"conversation_id": cid, "step_type": "agent_response", "text_delta": delta})
emit("result", result={"conversation_id": cid, "status": "SUCCESS", "response": ""})
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let invoke = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
            .arg("thread")
            .args(args)
            .current_dir(root)
            .env("PRIME_AGENT_CODING_AGENT_DIR", root.join("config"))
            .env("PA_ANTIGRAVITY_BIN", &script)
            .env("CAPTURE", root.join("args.json"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "args={args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };

    let providers = invoke(&["providers"]);
    assert!(providers.contains("antigravity\tavailable"), "{providers}");
    let models = invoke(&["models", "antigravity"]);
    assert!(
        models.contains("gemini-4-neon\tGemini 4 Neon\tlow,high"),
        "{models}"
    );

    let id = invoke(&["new", "antigravity"]);
    invoke(&[&id, "model", "gemini-4-neon"]);
    invoke(&[&id, "thinking", "xhigh"]);
    let reply1 = invoke(&[&id, "send", "first turn"]);
    assert_eq!(reply1, "antigravity reply");
    let captured1: Vec<String> =
        serde_json::from_str(&fs::read_to_string(root.join("args.json")).unwrap()).unwrap();
    assert!(captured1
        .windows(2)
        .any(|w| w == ["--model", "gemini-4-neon"]));
    assert!(captured1.windows(2).any(|w| w == ["--effort", "xhigh"]));
    assert!(captured1
        .iter()
        .any(|a| a.starts_with("-p=") && a.contains("first turn")));

    let reply2 = invoke(&[&id, "send", "second turn"]);
    assert_eq!(reply2, "antigravity reply");
    let captured2: Vec<String> =
        serde_json::from_str(&fs::read_to_string(root.join("args.json")).unwrap()).unwrap();
    assert!(captured2
        .windows(2)
        .any(|w| w == ["--conversation", "8b88c0dc-9360-4f7e-9c0c-22b969492f41"]));
    assert!(captured2.iter().any(|a| a == "-p=second turn"));
}

#[test]
fn cursor_provider_handles_models_thinking_and_chat_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let script = root.join("fake-cursor-agent");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if args == ["--version"]:
    print("2026.11.01")
    sys.exit(0)
if args == ["--list-models"]:
    print("\x1b[1mAvailable models\x1b[0m")
    print("composer-3 - Composer 3")
    print("gpt-6-low - GPT-6 Low")
    print("gpt-6-high - GPT-6 High\u200b")
    print("gpt-6-high - GPT-6 High")
    sys.exit(0)
pathlib.Path(os.environ["CAPTURE"]).write_text(json.dumps(args))
assert args[:6] == ["--print", "--output-format", "json", "--force", "--trust", "--workspace"], args
print(json.dumps({"type": "result", "subtype": "success", "is_error": False,
                  "result": "cursor reply\n", "session_id": "chat-123"}))
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let invoke = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
            .arg("thread")
            .args(args)
            .current_dir(root)
            .env("PRIME_AGENT_CODING_AGENT_DIR", root.join("config"))
            .env("PA_CURSOR_BIN", &script)
            .env("CAPTURE", root.join("args.json"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "args={args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let captured = || -> Vec<String> {
        serde_json::from_str(&fs::read_to_string(root.join("args.json")).unwrap()).unwrap()
    };

    let providers = invoke(&["providers"]);
    assert!(providers.contains("cursor\tavailable"), "{providers}");
    let models = invoke(&["models", "cursor"]);
    assert!(models.contains("composer-3\tComposer 3\t"), "{models}");
    assert!(models.contains("gpt-6\tGPT-6\tlow,high"), "{models}");
    assert_eq!(models.matches("gpt-6\t").count(), 1, "{models}");

    let id = invoke(&["new", "cursor"]);
    invoke(&[&id, "model", "gpt-6"]);
    invoke(&[&id, "thinking", "high"]);
    assert_eq!(invoke(&[&id, "send", "first turn"]), "cursor reply");
    let first = captured();
    assert!(
        first.windows(2).any(|w| w == ["--model", "gpt-6-high"]),
        "{first:?}"
    );
    assert!(!first.iter().any(|a| a == "--resume"), "{first:?}");
    let prompt = &first[first.iter().position(|a| a == "--").unwrap() + 1];
    assert!(prompt.starts_with("[Thread tool note:") && prompt.ends_with("first turn"));

    assert_eq!(invoke(&[&id, "send", "-- second turn"]), "cursor reply");
    let second = captured();
    assert!(
        second.windows(2).any(|w| w == ["--resume", "chat-123"]),
        "{second:?}"
    );
    assert_eq!(second.last().map(String::as_str), Some("-- second turn"));
}

/// The JSON commands behind pa's native history, machine editor, and review pages.
#[test]
fn workspace_pages_json_commands() {
    use serde_json::json;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let agent = root.join("agent");
    let machine = agent.join("machines/my-check");
    fs::create_dir_all(&machine).unwrap();
    fs::write(
        machine.join("machine.toml"),
        "format = 1\nname = \"my-check\"\nversion = 1\ndescription = \"Check\"\n[states.check]\nentry = true\nkind = \"python\"\nrun = \"check.py:check\"\noutputs = { result = \"text\" }\n",
    )
    .unwrap();
    fs::write(
        machine.join("check.py"),
        "def check(inputs):\n    return {'result': 'ok'}\n",
    )
    .unwrap();
    fs::create_dir_all(agent.join("sessions")).unwrap();
    fs::write(
        agent.join("sessions/s1.jsonl"),
        "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"hello history\"}}\n",
    )
    .unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    fs::write(repo.join("a.txt"), "one\n").unwrap();
    let thread = |cwd: &std::path::Path, args: &[&str], input: serde_json::Value| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
            .arg("thread")
            .args(args)
            .current_dir(cwd)
            .env("PRIME_AGENT_CODING_AGENT_DIR", &agent)
            .env("HOME", root)
            .env("DO_NOT_TRACK", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let reply = serde_json::from_slice::<serde_json::Value>(&out.stdout).ok();
        (
            out.status.success(),
            reply,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // History: rows by provider, then a row becomes a session pa can resume.
    let (ok, list, err) = thread(root, &["history", "--json"], json!({}));
    assert!(ok, "{err}");
    let row = &list.unwrap()["groups"][0]["rows"][0];
    assert_eq!(row["title"], "hello history");
    let (ok, opened, err) = thread(
        root,
        &["history", "--open"],
        json!({"target": row["target"], "title": "x"}),
    );
    assert!(ok, "{err}");
    assert!(opened.unwrap()["path"]
        .as_str()
        .unwrap()
        .ends_with("s1.jsonl"));

    // Machine editor: a draft that fails validation leaves the installed machine as it was.
    let draft = |name: &str| {
        let (ok, draft, err) = thread(root, &["fsm-edit", "draft"], json!({"name": name}));
        assert!(ok, "{err}");
        draft.unwrap()
    };
    let broken = draft("my-check");
    let file = broken["file"].as_str().unwrap().to_owned();
    assert!(
        !file.starts_with(agent.join("machines").to_str().unwrap()),
        "{file}"
    );
    fs::write(&file, fs::read_to_string(&file).unwrap() + "states = 3\n").unwrap();
    let (ok, _, err) = thread(
        root,
        &["fsm-edit", "save"],
        json!({"draft": broken["draft"]}),
    );
    assert!(!ok && err.contains("Validation failed"), "{err}");
    assert!(!fs::read_to_string(machine.join("machine.toml"))
        .unwrap()
        .contains("states = 3"));
    assert!(
        thread(
            root,
            &["fsm-edit", "discard"],
            json!({"draft": broken["draft"]})
        )
        .0
    );
    let good = draft("my-check");
    let file = good["file"].as_str().unwrap().to_owned();
    fs::write(
        &file,
        fs::read_to_string(&file)
            .unwrap()
            .replace("\"Check\"", "\"Checked\""),
    )
    .unwrap();
    let (ok, saved, err) = thread(root, &["fsm-edit", "save"], json!({"draft": good["draft"]}));
    assert!(ok, "{err}");
    assert_eq!(saved.unwrap(), json!({"saved": "my-check"}));
    assert!(fs::read_to_string(machine.join("machine.toml"))
        .unwrap()
        .contains("\"Checked\""));

    // Review: files, then staging moves the file to the staged view.
    let (ok, files, err) = thread(&repo, &["review", "json"], json!({"op": "files"}));
    assert!(ok, "{err}");
    assert_eq!(
        files.unwrap()["files"],
        json!([{"path": "a.txt", "status": "?"}])
    );
    assert!(
        thread(
            &repo,
            &["review", "json"],
            json!({"op": "stage", "path": "a.txt"})
        )
        .0
    );
    let (_, staged, _) = thread(&repo, &["review", "json"], json!({"op": "files"}));
    assert_eq!(staged.unwrap()["staged"], true);
}
