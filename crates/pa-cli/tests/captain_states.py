"""Captain stop, resume, and delete: states, running work, digest wakeups, and restart.
Usage: python3 captain_states.py PATH_TO_PA [TEMP_DIR]"""
import atexit, json, os, pathlib, signal, socket, sqlite3, subprocess, sys, tempfile, threading, time

P = pathlib.Path
binary = str(P(sys.argv[1]).resolve())
temp = P(tempfile.mkdtemp(prefix="pa-captain-states-", dir=sys.argv[2] if len(sys.argv) > 2 else "/tmp")).resolve()
a = temp / "alpha"; a.mkdir()
hub = temp / "hub"; native = temp / "native"; native.mkdir()
mock = temp / "bin"; mock.mkdir()

# A provider CLI that answers at once, or holds the turn open for a "slow" task.
(mock / "claude").write_text("""#!/usr/bin/env python3
import json, sys, time
body = sys.stdin.read()
with open(__file__ + ".log", "a") as log:
    log.write(json.dumps(body) + "\\n")
if "slow" in body and "is your assignment" in body:
    time.sleep(120)
if "FAILTURN" in body:
    sys.exit("provider failed")
print(json.dumps({"type": "result", "session_id": "fa16e11d-0a87-47ae-a8d1-f6e3a12b806f", "result": "crew done", "is_error": False}))
""")
(mock / "claude").chmod(0o755)

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
(d / "nodes.py").write_text("def scan(inputs):\n    import time\n    time.sleep(120)\n    return {'done': 'yes'}\n")
(d / "captain.toml").write_text('access = "read"\n')
(native / "settings.json").write_text(json.dumps(dict(factory=dict(enabled=True), onboardingShown=True, agentTraces=dict(enabled=False))))

# Prime turns run on this daemon; the macOS socket path limit is 104 bytes.
SOCK = f"/tmp/pa-cs-{os.getpid()}.sock"
atexit.register(subprocess.run, ["pkill", "-f", SOCK], check=False)
env = dict(os.environ, PA_CAPTAIN_HOME=str(hub), PRIME_AGENT_CODING_AGENT_DIR=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native),
           PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"), PA_CAPTAIN_NATIVE_SOCKET=SOCK,
           DO_NOT_TRACK="1", PATH=str(mock) + ":" + os.environ["PATH"],
           PRIME_AGENT_FAUX_SCRIPT=json.dumps({"responses": ["Captain received the report."] * 12}))
for key in ("PRIME_AGENT_KERNEL_PYTHON", "PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION"):
    env.pop(key, None)


def run(*args, body=None, allow=False):
    cp = subprocess.run([binary, "thread", *args], cwd=a, env=env, input=body, text=True, capture_output=True, timeout=120)
    if not allow and cp.returncode:
        raise RuntimeError(str(args) + " " + cp.stderr[-1500:])
    return cp


def act(op, **args): return json.loads(run("captain", "act", op, body=json.dumps(args)).stdout)
def fails(op, **args):
    cp = run("captain", "act", op, body=json.dumps(args), allow=True)
    assert cp.returncode, (op, cp.stdout)
    return cp.stderr
def stat(): return json.loads(run("captain", "status").stdout)
def db(): return sqlite3.connect(hub / "captains/projects.sqlite3")
def state(pid): return db().execute("select state from projects where id=?", (pid,)).fetchone()


def wait(f, limit=90, what=""):
    for _ in range(int(limit / 0.25)):
        res = f()
        if res:
            return res
        time.sleep(0.25)
    with db() as c:
        raise RuntimeError(f"timeout {what} " + str(c.execute("select purpose,status,result from jobs").fetchall()))


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


print(temp, flush=True)
try:
    subprocess.check_call(["git", "-C", str(a), "init", "-q"])
    subprocess.check_call(["git", "-C", str(a), "-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "--allow-empty", "-qm", "base"])
    pid = act("init", objective="Ship Alpha")["id"]
    wait(lambda: db().execute("select 1 from jobs where purpose='brief' and status='done'").fetchone(), what="brief")
    lead = db().execute("select lead from projects").fetchone()[0]
    assert stat()["projects"][0]["state"] == "active"
    assert stat()["projects"][0]["lead_session"], "a Prime lead opens as a pa chat"
    status_dir = hub / "captains" / "status"
    idx = json.loads((status_dir / "index.json").read_text())
    assert idx.get(str(a.resolve())) == pid, idx
    pstatus = json.loads((status_dir / f"{pid}.json").read_text())
    assert pstatus["state"] == "active" and pstatus["project_id"] == pid, pstatus

    # Stop with a crew turn and a Captain-started factory run in flight.
    crew = act("delegate", project=pid, provider="claude", task="slow review")
    wait(lambda: db().execute("select pid from jobs where id=? and status='running'", (crew["job"],)).fetchone(), what="crew turn")
    worker = db().execute("select pid from jobs where id=?", (crew["job"],)).fetchone()[0]
    scan = act("delegate", project=pid, machine="slow-scan", task="Scan")
    control = hub / "captains" / "runs" / scan["id"]
    wait(lambda: (control / "run.json").exists() and "run" in json.loads((control / "run.json").read_text()), what="scan start")
    pstatus_live = json.loads((status_dir / f"{pid}.json").read_text())
    assert pstatus_live["counts"]["working"] >= 1, pstatus_live
    assert pstatus_live["counts"]["factory_running"] >= 1, pstatus_live
    queued = act("delegate", project=pid, provider="claude", task="queued later")
    stopped = json.loads(run("captain", "stop").stdout)
    assert stopped["state"] in ("stopping", "paused"), stopped
    wait(lambda: state(pid)[0] == "paused", what="paused")
    assert not alive(worker), "the crew turn's worker still runs"
    with db() as c:
        assert c.execute("select status,result from jobs where id=?", (crew["job"],)).fetchone() == ("stopped", "Stopped by the user.")
        assert c.execute("select status from jobs where id=?", (queued["job"],)).fetchone()[0] == "cancelled"
        assert c.execute("select state from machines where member=?", (scan["id"],)).fetchone()[0] == "stopped"
        assert not c.execute("select 1 from jobs where project=? and status in ('queued','running')", (pid,)).fetchone()
    assert json.loads((hub / "threads" / (crew["id"] + ".json")).read_text())["pending"] is None
    assert stat()["projects"][0]["state"] == "paused"
    pstatus_paused = json.loads((status_dir / f"{pid}.json").read_text())
    assert pstatus_paused["state"] == "paused", pstatus_paused
    print("Verified stop ends a running crew turn and a factory run, cancels queued work, and pauses the Captain.", flush=True)

    # A paused Captain starts nothing: no digest wakeup, no message, no assignment.
    adopted = run("new", "claude").stdout.strip()
    act("adopt", project=pid, ref=adopted)
    run(adopted, "send", "fresh crew report")
    for _ in range(3):
        stat()
    time.sleep(1)
    with db() as c:
        assert not c.execute("select 1 from jobs where project=? and purpose='digest' and status in ('queued','running')", (pid,)).fetchone()
    assert "paused" in fails("lead", project=pid, text="hello")
    assert "paused" in fails("delegate", project=pid, provider="claude", task="x")
    again = json.loads(run("captain", "stop").stdout)
    assert again["state"] == "paused", again
    print("Verified a paused Captain blocks digest wakeups and new work, and stop is idempotent.", flush=True)

    # Resume starts work again.
    assert act("resume", project=pid)["state"] == "active"
    assert act("resume", project=pid)["state"] == "active"
    job = act("lead", project=pid, text="Status please")["job"]
    wait(lambda: db().execute("select 1 from jobs where id=? and status='done'", (job,)).fetchone(), what="lead after resume")
    print("Verified resume reactivates the Captain.", flush=True)

    def lead_idle():
        return not db().execute("select 1 from jobs where member=? and status in ('queued','running')", (lead,)).fetchone()

    def lead_turn(text, idle=True, **extra):
        if idle:
            wait(lead_idle, what="idle lead before " + text)
        jid = act("lead", project=pid, text=text, **extra)["job"]
        wait(lambda: db().execute("select 1 from jobs where id=? and status not in ('queued','running')", (jid,)).fetchone(), what="lead " + text)
        return db().execute("select status from jobs where id=?", (jid,)).fetchone()[0]

    def hidden():
        session = stat()["projects"][0]["lead_session"]
        rows = [json.loads(line) for line in P(session).read_text().splitlines()]
        return [r["content"] for r in rows if r.get("customType") == "hidden_context"]

    def crew_turn(task):
        job = act("delegate", project=pid, provider="claude", task=task, request_id="t-" + task)
        wait(lambda: db().execute("select 1 from jobs where id=? and status='done'", (job["job"],)).fetchone(), what=task)
        return job["id"]

    def cursor():
        return db().execute("select last_report_rowid from projects where id=?", (pid,)).fetchone()[0]

    # Delta context: a second turn in the same session repeats no unchanged Drive text, crew
    # rows are one line each, and reports arrive as one-line pointers to their files.
    assert act("wake-policy", project=pid)["policy"]["interrupted"] == "ignore"
    act("wake-policy", project=pid, policy={"done": "context"})
    act("edit", project=pid, field="plan", value="PLANTEXT " * 120)
    members = [crew_turn(f"review area {i}") for i in range(3)]
    time.sleep(1.5)
    with db() as c:
        assert not c.execute("select 1 from jobs where purpose='digest' and status in ('queued','running')").fetchone(), "a context event woke the Captain"
        kinds = c.execute("select event_kind,report_path from reports where member=?", (members[0],)).fetchall()
    assert kinds and kinds[0][0] == "done" and (a / kinds[0][1]).is_file(), kinds
    assert (a / kinds[0][1]).read_text().strip().endswith("crew done")
    inbox = act("notifications", project=pid)
    assert inbox["unread_notifications"] >= 3 and inbox["notifications"][0]["kind"] == "done", inbox
    assert stat()["projects"][0]["unread_notifications"] == inbox["unread_notifications"]
    assert lead_turn("First status") == "done"
    first = hidden()[-1]
    assert "PLANTEXT" in first and "Crew (handle | name | state | diff | step or need):" in first, first[-2000:]
    for mid in members:
        assert f"[{mid[:8]} " in first and f"(full: .prime/captain/reports/{mid[:8]}-" in first, first[-2000:]
    advanced = cursor()
    assert advanced > 0
    assert lead_turn("Second status") == "done"
    second = hidden()[-1]
    assert "PLANTEXT" not in second and "Drive .prime/captain/NOTES.md" in second and "No new crew reports." in second, second
    assert len(second.encode()) * 2 < len(first.encode()), (len(first), len(second))
    print(f"Verified delta context: {len(first.encode())} then {len(second.encode())} bytes of hidden context.", flush=True)

    # Configured models ride the cache-stable menus: sent on the session's first turn, not repeated unchanged.
    listed = subprocess.run([binary, "model", "list"], env=dict(env, PRIME_AGENT_CODING_AGENT_DIR=str(native)),
                            capture_output=True, text=True, timeout=60).stdout.splitlines()[1:]
    models_line = "Configured models (use only these, as provider/model): "
    opening = hidden()[0]
    if listed:
        provider = listed[0].split()[0]
        line = next((l for l in opening.splitlines() if l.startswith(models_line)), opening[-2500:])
        assert f"{provider}: " in line and len(line) < 700, line
    assert models_line not in first and models_line not in second, second[-1500:]
    assert "Captain instructions and menus" in second, second[-1500:]
    guide = P(__file__).resolve().parents[3] / "skills/captain/references/fsm-design-guide.md"
    assert guide.is_file() and "route" in guide.read_text()
    wait(lead_idle, what="idle lead before design")
    design = act("factory-design", project=pid, text="Design a delivery pipeline.", request_id="design-guide")["job"]
    wait(lambda: db().execute("select 1 from jobs where id=? and status='done'", (design,)).fetchone(), what="design turn")
    designed = hidden()[-1]
    assert f"Read the full FSM authoring & archetype guide at {guide}" in designed, designed[-2500:]
    assert "rlm.factory.help()" in designed and (models_line in designed) == bool(listed), designed[-2500:]
    print(f"Verified configured models ({len(line) if listed else 0} chars) and the FSM guide reach design turns.", flush=True)

    # A failed turn keeps its reports for the next turn.
    act("edit", project=pid, field="model", provider="claude", value="")
    late = crew_turn("review area late")
    advanced = cursor()
    assert lead_turn("FAILTURN now") == "needs_you"
    assert cursor() == advanced, "a failed turn advanced the report cursor"
    run(lead, "abandon")
    assert lead_turn("After failure") == "done"
    log = [json.loads(line) for line in (mock / "claude.log").read_text().splitlines()]
    assert f"[{late[:8]} " in log[-1] and cursor() > advanced, log[-1][-1500:]
    act("edit", project=pid, field="model", provider="prime", value="")
    print("Verified a failed Captain turn does not advance the report cursor.", flush=True)

    # A watcher from before event kinds writes reports with an empty kind; none are dropped.
    with db() as c:
        c.execute("insert into reports(project,id,member,state,detail,stamp) values(?,?,?,'done','LEGACY-REPORT finished',?)",
                  (pid, members[1] + "/legacy", members[1], time.time()))
    assert lead_turn("Legacy status") == "done"
    assert f"[{members[1][:8]} " in hidden()[-1] and "done: LEGACY-REPORT finished" in hidden()[-1], hidden()[-1][-1500:]
    print("Verified report_delta delivers legacy reports with an empty event kind.", flush=True)

    # A user turn cancels a queued, debounced digest; it reads the same reports.
    act("wake-policy", project=pid, policy={"done": "react", "debounce_seconds": 60})
    crew_turn("review area debounce")
    wait(lambda: db().execute("select id from jobs where purpose='digest' and status='queued' and not_before!=''").fetchone(), what="debounced digest")
    digest = db().execute("select id from jobs where purpose='digest' and status='queued'").fetchone()[0]
    assert lead_turn("Covering status", idle=False) == "done"
    assert db().execute("select status from jobs where id=?", (digest,)).fetchone()[0] == "cancelled"
    act("wake-policy", project=pid, policy={"debounce_seconds": 0})
    print("Verified context events skip the wakeup and a user turn cancels a queued digest.", flush=True)

    # The inbox, unread markers, and opening a factory step's session directly.
    one = inbox["notifications"][0]["id"]
    assert act("notifications-read", project=pid, id=one)["read"] == 1
    assert act("notifications-read", project=pid)["read"] >= 1
    assert act("notifications", project=pid)["unread_notifications"] == 0
    assert act("mark-read", project=pid, member=members[0])["marked"] == 1
    assert [m for m in stat()["projects"][0]["crew"] if m["id"] == members[0]][0]["unread_count"] == 0
    step = temp / "step.jsonl"
    step.write_text("{}\n")
    opened = act("open", project=pid, member="step:" + str(step))
    assert opened == {"ok": True, "kind": "native", "session": str(step), "cwd": str(a.resolve())}, opened
    assert "No live step session" in fails("open", project=pid, member="step:sub-missing1")
    assert "usage" in stat()["projects"][0]
    print("Verified notifications, mark-read, and step-open.", flush=True)

    # Restart during stopping: a stop interrupted before it settled finishes once on reconcile.
    crew2 = act("delegate", project=pid, provider="claude", task="slow second")
    wait(lambda: db().execute("select 1 from jobs where id=? and status='running'", (crew2["job"],)).fetchone(), what="second crew turn")
    worker2 = db().execute("select pid from jobs where id=?", (crew2["job"],)).fetchone()[0]
    with db() as c:
        c.execute("update projects set state='stopping' where id=?", (pid,))
    os.killpg(os.getpgid(worker2), signal.SIGKILL)
    for _ in range(3):
        stat()
    assert state(pid)[0] == "paused"
    with db() as c:
        rows = c.execute("select status from jobs where id=?", (crew2["job"],)).fetchall()
        assert rows == [("stopped",)], rows
    print("Verified a restart during stopping reconciles to paused once.", flush=True)

    # A new blank crew chat: a native Prime session in the project checkout, user-only.
    fresh = act("new-thread", project=pid, title="Quick chat")
    assert fresh["ok"] and fresh["cwd"] == str(a.resolve()), fresh
    header = json.loads(P(fresh["session"]).read_text().splitlines()[0])
    assert header["type"] == "session" and header["id"] == fresh["member"] and header["cwd"] == fresh["cwd"], header
    with db() as c:
        assert c.execute("select kind,provider,title,ref from crew where id=?", (fresh["member"],)).fetchone() == (
            "native", "prime", "Quick chat", fresh["session"])
    listed = [m for m in stat()["projects"][0]["crew"] if m["id"] == fresh["member"]]
    assert listed and listed[0]["title"] == "Quick chat", stat()["projects"][0]["crew"]
    env["PA_CAPTAIN_ACTOR"] = "crew:" + pid
    try:
        said = fails("new-thread", project=pid)
        assert "crew thread cannot" in said, said
    finally:
        env.pop("PA_CAPTAIN_ACTOR")
    # Archive refuses a thread whose daemon turn still runs, unless forced, which aborts it.
    fake = f"/tmp/pa-cs-fake-{os.getpid()}.sock"
    aborted = []
    server = socket.socket(socket.AF_UNIX)
    server.bind(fake)
    server.listen()

    def serve():
        while True:
            try:
                conn, _ = server.accept()
            except OSError:
                return
            with conn:
                frame = json.loads(conn.makefile().readline())
                command = frame["command"]
                if command["type"] == "abort":
                    aborted.append(command)
                data = {"sessions": [{"id": "s1", "sessionFile": fresh["session"], "isStreaming": True}]}
                conn.sendall((json.dumps({"type": "response", "id": frame["id"], "success": True, "data": data}) + "\n").encode())

    threading.Thread(target=serve, daemon=True).start()
    real_sock = env["PA_CAPTAIN_NATIVE_SOCKET"]
    env["PA_CAPTAIN_NATIVE_SOCKET"] = fake
    try:
        said = fails("archive", project=pid, member=fresh["member"])
        assert "still has an active daemon turn" in said, said
        assert db().execute("select archived from crew where id=?", (fresh["member"],)).fetchone()[0] == 0
        assert act("archive", project=pid, member=fresh["member"], force=True)["archived"] is True
        assert aborted and aborted[0]["activeSessionId"] == "s1", aborted
    finally:
        env["PA_CAPTAIN_NATIVE_SOCKET"] = real_sock
        server.close()
        os.unlink(fake)
    act("unarchive", project=pid, member=fresh["member"])
    print("Verified archive refuses a busy daemon turn and force aborts it first.", flush=True)
    act("release", project=pid, member=fresh["member"])
    print("Verified new-thread opens a blank Prime crew chat and refuses a crew actor.", flush=True)

    # Delete while work runs: stop first, remove the record, keep threads and transcripts.
    assert act("resume", project=pid)["state"] == "active"
    crew3 = act("delegate", project=pid, provider="claude", task="slow third", write=True)
    wait(lambda: db().execute("select 1 from jobs where id=? and status='running'", (crew3["job"],)).fetchone(), what="third crew turn")
    worker3 = db().execute("select pid from jobs where id=?", (crew3["job"],)).fetchone()[0]
    tree = P(db().execute("select cwd from crew where id=?", (crew3["id"],)).fetchone()[0])
    deleted = json.loads(run("captain", "delete").stdout)
    assert deleted["state"] == "deleted" and deleted["lead"] == lead, deleted
    assert not alive(worker3)
    with db() as c:
        for table in ("projects", "crew", "jobs"):
            assert not c.execute(f"select 1 from {table}").fetchone(), table
    for tid in (lead, crew["id"], adopted):
        assert (hub / "threads" / (tid + ".json")).is_file(), tid
    assert json.loads((hub / "threads" / (adopted + ".json")).read_text())["messages"], "transcript kept"
    assert tree.is_dir(), "worktree kept"
    assert stat()["projects"] == []
    assert not (status_dir / f"{pid}.json").exists(), "status file removed on delete"
    assert json.loads((status_dir / "index.json").read_text()) == {}, "index cleared on delete"
    assert run("captain", "delete", allow=True).returncode, "nothing left to delete"
    print("Verified delete stops running work, removes the Captain, and keeps threads, transcripts, and worktrees.", flush=True)
finally:
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as c:
            c.execute("delete from watches")
    subprocess.run(["pkill", "-f", str(temp)], check=False)
