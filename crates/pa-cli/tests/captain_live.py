"""Captain turns run as sessions of the shared daemon: a chat opens live while a turn runs, its
subagents are in the session roster, a stop aborts the turn, and the actor of a Captain or crew
turn holds even when the tool environment lacks PA_CAPTAIN_ACTOR.
Usage: python3 captain_live.py PATH_TO_PA [TEMP_DIR]"""
import atexit, json, os, pathlib, sqlite3, subprocess, sys, tempfile, time

P = pathlib.Path
sys.path.insert(0, str(P(__file__).resolve().parent))
from captain_daemon import Connection, live_session, request  # noqa: E402

binary = str(P(sys.argv[1]).resolve())
temp = P(tempfile.mkdtemp(prefix="pa-captain-live-", dir=sys.argv[2] if len(sys.argv) > 2 else "/tmp")).resolve()
a = temp / "alpha"; a.mkdir()
hub = temp / "hub"; native = temp / "native"; native.mkdir()
FAUX = temp / "faux.json"
RESULTS = temp / "results"; RESULTS.mkdir()
RELEASE = temp / "release"
# The macOS socket path limit is 104 bytes.
SOCK = f"/tmp/pa-cl-{os.getpid()}.sock"
atexit.register(subprocess.run, ["pkill", "-f", SOCK], check=False)

d = native / "machines" / "slow-scan"
d.mkdir(parents=True)
(d / "machine.toml").write_text('''format = 1
name = "slow-scan"
version = 1
description = "Scan slowly."
[states.scan]
entry = true
kind = "python"
run = "nodes.py:scan"
outputs = { done = "text" }
''')
(d / "nodes.py").write_text("def scan(inputs):\n    return {'done': 'yes'}\n")
(native / "settings.json").write_text(json.dumps(dict(factory=dict(enabled=True), onboardingShown=True, agentTraces=dict(enabled=False))))

env = dict(os.environ, PA_CAPTAIN_HOME=str(hub), PRIME_AGENT_CODING_AGENT_DIR=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native),
           PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"), PA_CAPTAIN_NATIVE_SOCKET=SOCK, DO_NOT_TRACK="1",
           PRIME_AGENT_FAUX_SCRIPT=str(FAUX))
for key in ("PRIME_AGENT_KERNEL_PYTHON", "PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION", "RLM_SESSION_ID"):
    env.pop(key, None)


def run(*args, body=None, allow=False):
    cp = subprocess.run([binary, "thread", *args], cwd=a, env=env, input=body, text=True, capture_output=True, timeout=120)
    if not allow and cp.returncode:
        raise RuntimeError(str(args) + " " + cp.stderr[-1500:])
    return cp


def act(op, **args): return json.loads(run("captain", "act", op, body=json.dumps(args)).stdout)
def db(): return sqlite3.connect(hub / "captains/projects.sqlite3")


def wait(f, limit=120, what=""):
    for _ in range(int(limit / 0.25)):
        res = f()
        if res:
            return res
        time.sleep(0.25)
    with db() as c:
        raise RuntimeError(f"timeout {what} " + str(c.execute("select purpose,status,result from jobs").fetchall()))


def cell(code):
    return {"content": [{"type": "toolCall", "name": "python_repl", "arguments": {"code": code}}]}


def script(responses, children=None):
    """The faux script of the next daemon worker, and of the subagents its turns spawn."""
    FAUX.write_text(json.dumps({"engine": "faux", "responses": responses, "repeatLastResponse": True}))
    if children:
        child = temp / f"children-{len(list(temp.glob('children-*')))}.json"
        child.write_text(json.dumps({"engine": "faux", "responses": children, "repeatLastResponse": True}))
        env["PRIME_AGENT_FAUX_CHILD_SCRIPT"] = str(child)


def attempts(tag, ops):
    """A cell that runs Captain actions from the session's shell, once with the environment the
    session has and once without PA_CAPTAIN_ACTOR (a worker that a pa chat started), and writes
    each exit code and output to RESULTS/<tag>.json."""
    program = temp / f"attempts-{tag}.py"
    program.write_text(f"""import json, os, sqlite3, subprocess
project = sqlite3.connect({str(hub / "captains/projects.sqlite3")!r}).execute("select id from projects").fetchone()[0]
out = {{}}
for label, base in (("env", dict(os.environ)), ("no-actor", {{k: v for k, v in os.environ.items() if k != "PA_CAPTAIN_ACTOR"}})):
    for op, args in {json.dumps(ops)}:
        args = dict(args, project=project)
        cp = subprocess.run([{binary!r}, "thread", "captain", "act", op], cwd={str(a)!r}, env=base,
                            input=json.dumps(args), text=True, capture_output=True)
        out[label + " " + op] = [cp.returncode, (cp.stdout + cp.stderr)[-400:]]
out["actor"] = os.environ.get("PA_CAPTAIN_ACTOR")
out["session"] = os.environ.get("RLM_SESSION_ID")
open({str(RESULTS / (tag + '.json'))!r}, "w").write(json.dumps(out))
""")
    done = RESULTS / (tag + ".json")
    return (f"import asyncio, os\nr = await bash('python3 {program}')\n"
            f"for _ in range(1200):\n    if os.path.exists({str(done)!r}):\n        break\n    await asyncio.sleep(0.1)\nprint(r)")


def results(tag):
    path = RESULTS / (tag + ".json")
    wait(path.exists, what="results " + tag)
    return json.loads(path.read_text())


def lead_session():
    lead = db().execute("select lead from projects").fetchone()[0]
    return json.loads((hub / "threads" / (lead + ".json")).read_text())["native"]


DIGEST = "0" * 64
print(temp, flush=True)
try:
    subprocess.check_call(["git", "-C", str(a), "init", "-q"])
    subprocess.check_call(["git", "-C", str(a), "-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "--allow-empty", "-qm", "base"])

    # The first turn spawns a subagent, then works in a slow tool cell that prints as it goes.
    first = (f"import pathlib, time\nchild = await rlm.spawn('probe the tree', name='probe')\nprint('SPAWNED', child.rlm_child_id, flush=True)\n"
             f"while not pathlib.Path({str(RELEASE)!r}).exists():\n    print('TICK', flush=True)\n    time.sleep(0.3)\n"
             f"pathlib.Path({str(RESULTS / 'finished')!r}).write_text('yes')")
    digest_ops = [["factory-approve", {"name": "proj-build", "digest": DIGEST}], ["grant", {"machine": "slow-scan"}]]
    script([cell(first), "Brief done.", cell(attempts("digest", digest_ops)), "Digest noted."],
           children=["probe done"])
    pid = act("init", objective="Ship Alpha")["id"]
    wait(lambda: db().execute("select 1 from jobs where purpose='brief' and status='running'").fetchone(), what="brief start")
    session = lead_session()
    row = wait(lambda: (lambda r: r if r and r.get("isRunningTools") else None)(live_session(SOCK, session)), what="tool cell")
    assert row["isStreaming"], row

    # Open the chat the way pa does: create on the session file, then attach. The daemon answers
    # with the live worker; no other process holds the file's lease.
    chat = Connection(SOCK)
    created = chat.request({"type": "create", "sessionPath": session, "config": {"cwd": str(a)}})
    assert created["success"], created
    active = created["data"].get("activeSessionId") or created["data"]["id"]
    assert active == row["activeSessionId"], (created, row)
    attached = chat.request({"type": "attach", "activeSessionId": active})
    assert attached["success"], attached
    seen = []
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and not any("TICK" in json.dumps(f) for f in seen):
        seen.append(chat.frame())
    assert any("TICK" in json.dumps(f) for f in seen), [json.dumps(f)[:200] for f in seen[-5:]]
    print("Verified the Captain chat opens on the daemon while its turn runs and streams the tool output.", flush=True)

    roster = wait(lambda: (lambda r: r if r.get("data", {}).get("children") else None)(
        request(SOCK, {"type": "get_rlm_children", "activeSessionId": active})), what="subagent roster")
    assert any("probe" in json.dumps(c) for c in roster["data"]["children"]), roster
    print("Verified a subagent the turn spawned is in the chat's subagent roster.", flush=True)

    # A stop aborts the running turn in the daemon and settles the job.
    stopped = json.loads(run("captain", "stop").stdout)
    assert stopped["state"] in ("stopping", "paused"), stopped
    wait(lambda: db().execute("select state from projects").fetchone()[0] == "paused", what="paused")
    with db() as c:
        assert c.execute("select status,result from jobs where purpose='brief'").fetchone() == ("stopped", "Stopped by the user.")
    wait(lambda: not (live_session(SOCK, session) or {}).get("isStreaming"), what="turn aborted")
    lead = db().execute("select lead from projects").fetchone()[0]
    assert json.loads((hub / "threads" / (lead + ".json")).read_text())["pending"] is None
    RELEASE.write_text("go")
    time.sleep(1)
    assert not (RESULTS / "finished").exists(), "the stopped cell ran on"
    assert not subprocess.run(["pgrep", "-f", f"--resume {session}"], capture_output=True).stdout.strip(), "the turn client still runs"
    chat.close()
    print("Verified stop aborts the daemon turn and settles the job.", flush=True)

    # A crew turn on the daemon cannot run user-only actions, with or without PA_CAPTAIN_ACTOR.
    act("resume", project=pid)
    crew_ops = [["factory-approve", {"name": "proj-build", "digest": DIGEST}], ["grant", {"machine": "slow-scan"}],
                ["factory-delete", {"name": "proj-build"}],
                ["specialist-write", {"id": "verifier", "name": "Verifier", "description": "d", "model": "", "thinking": "", "prompt": "p"}]]
    # A subagent of the crew turn has the crew's actor too.
    spawn = (f"import asyncio, os\nc = await rlm.spawn('help', name='helper')\n"
             f"for _ in range(1200):\n    if os.path.exists({str(RESULTS / 'child.json')!r}):\n        break\n    await asyncio.sleep(0.1)")
    script([cell(attempts("crew", crew_ops)), cell(spawn), "crew done"],
           children=[cell(attempts("child", crew_ops[:2])), "helped"])
    crew = act("delegate", project=pid, provider="prime", task="Check the tree", request_id="crew-1")
    out = results("crew")
    # The daemon worker never carries the actor; the session's records give it.
    assert out["actor"] is None, out
    for label in ("env", "no-actor"):
        for op, _ in crew_ops:
            code, text = out[f"{label} {op}"]
            assert code and "crew thread cannot" in text, (label, op, text)
    out = results("child")
    for label in ("env", "no-actor"):
        for op, _ in crew_ops[:2]:
            code, text = out[f"{label} {op}"]
            assert code and "crew thread cannot" in text, ("child", label, op, text)
    assert not (a / ".prime" / "agents" / "verifier.md").exists()
    wait(lambda: db().execute("select 1 from jobs where id=? and status='done'", (crew["job"],)).fetchone(), what="crew done")
    print("Verified a crew turn on the daemon and its subagent cannot approve, grant, delete, or write a specialist.", flush=True)

    # The crew report wakes a digest turn in the lead's worker; it cannot approve or grant.
    out = results("digest")
    for label in ("env", "no-actor"):
        code, text = out[label + " factory-approve"]
        assert code and "Only the user approves" in text, (label, text)
        code, text = out[label + " grant"]
        assert code and "only in a turn that answers a user message" in text, (label, text)
    assert json.loads(db().execute("select grants from projects").fetchone()[0]) == []
    print("Verified a lead digest turn cannot approve a factory or grant a machine.", flush=True)

    # A user turn in an ordinary pa chat on the same daemon still can.
    user_script = temp / "user-faux.json"
    user_ops = [["grant", {"machine": "slow-scan"}]]
    user_script.write_text(json.dumps({"engine": "faux", "responses": [cell(attempts("user", user_ops)), "ok"], "repeatLastResponse": True}))
    user_env = dict(env, PRIME_AGENT_CODING_AGENT_DIR=str(native), PRIME_AGENT_DAEMON_SOCKET=SOCK, PRIME_AGENT_FAUX_SCRIPT=str(user_script))
    user = subprocess.Popen([binary, "--mode", "acp", "--cwd", str(a)], cwd=a, env=user_env, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=open(temp / "user.err", "w"), text=True)
    try:
        def acp(i, method, params):
            user.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n")
            user.stdin.flush()
            for line in user.stdout:
                message = json.loads(line)
                if message.get("id") == i:
                    assert "error" not in message, message
                    return message["result"]
            raise RuntimeError("the user chat stopped: " + (temp / "user.err").read_text()[-1500:])
        acp(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        sid = acp(2, "session/new", {"cwd": str(a), "mcpServers": []})["sessionId"]
        acp(3, "session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "Allow slow-scan."}]})
    finally:
        user.stdin.close()
        user.wait(timeout=30)
    out = results("user")
    assert out["env grant"][0] == 0 and '"by": "user"' in out["env grant"][1], out
    assert json.loads(db().execute("select grants from projects").fetchone()[0]) == ["slow-scan"]
    print("Verified a user turn in an ordinary chat on the daemon can still grant.", flush=True)

    # A daemon turn that fails, or that pa aborts (Esc), leaves the thread ready for its next message.
    def settled(job):
        return wait(lambda: (lambda r: r if r and r[0] not in ("queued", "running") else None)(
            db().execute("select status,result from jobs where id=?", (job,)).fetchone()), what="job " + job)

    def thread(member):
        return json.loads((hub / "threads" / (member + ".json")).read_text())

    script([{"error": "provider boom"}])
    failed = act("delegate", project=pid, provider="prime", task="Fail once", request_id="fail-1")
    status, result = settled(failed["job"])
    assert thread(failed["id"])["pending"] is None, (status, result)
    again = act("reply", project=pid, member=failed["id"], text="Try again")
    status, result = settled(again["job"])
    assert "unfinished turn" not in (result or ""), result
    print("Verified a failed daemon turn leaves the crew thread ready for its next message.", flush=True)

    release = temp / "release-abort"
    slow = (f"import pathlib, time\nwhile not pathlib.Path({str(release)!r}).exists():\n    print('WAIT', flush=True)\n    time.sleep(0.3)")
    script([cell(slow), "after abort"])
    aborted = act("delegate", project=pid, provider="prime", task="Wait for Esc", request_id="abort-1")
    crew_session = wait(lambda: (hub / "threads" / (aborted["id"] + ".json")).exists() and thread(aborted["id"]).get("native"),
                        what="crew session")
    row = wait(lambda: (lambda r: r if r and r.get("isRunningTools") else None)(live_session(SOCK, crew_session)), what="crew tool cell")
    assert request(SOCK, {"type": "abort", "activeSessionId": row["activeSessionId"]})["success"]
    status, result = settled(aborted["job"])
    release.write_text("go")
    assert thread(aborted["id"])["pending"] is None, (status, result)
    assert thread(aborted["id"])["native"] == crew_session, "the aborted thread keeps its daemon session"
    again = act("reply", project=pid, member=aborted["id"], text="Carry on")
    status, result = settled(again["job"])
    assert status == "done", (status, result)
    print("Verified an aborted daemon turn leaves the crew thread ready and on the same session.", flush=True)
finally:
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as c:
            c.execute("delete from watches")
        run("captain", "stop", allow=True)
