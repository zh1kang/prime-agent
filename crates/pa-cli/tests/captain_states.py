"""Captain stop, resume, and delete: states, running work, digest wakeups, and restart.
Usage: python3 captain_states.py PATH_TO_PA [TEMP_DIR]"""
import json, os, pathlib, signal, sqlite3, subprocess, sys, tempfile, time

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
if "slow" in body:
    time.sleep(120)
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

env = dict(os.environ, PA_CAPTAIN_HOME=str(hub), PRIME_AGENT_CODING_AGENT_DIR=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native),
           PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"), PA_CAPTAIN_NATIVE_SOCKET=str(temp / "native.sock"),
           DO_NOT_TRACK="1", PATH=str(mock) + ":" + os.environ["PATH"],
           PRIME_AGENT_FAUX_SCRIPT=json.dumps({"responses": ["Captain received the report."] * 8}))
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

    # Stop with a crew turn and a Captain-started factory run in flight.
    crew = act("delegate", project=pid, provider="claude", task="slow review")
    wait(lambda: db().execute("select pid from jobs where id=? and status='running'", (crew["job"],)).fetchone(), what="crew turn")
    worker = db().execute("select pid from jobs where id=?", (crew["job"],)).fetchone()[0]
    scan = act("delegate", project=pid, machine="slow-scan", task="Scan")
    control = hub / "captains" / "runs" / scan["id"]
    wait(lambda: (control / "run.json").exists() and "run" in json.loads((control / "run.json").read_text()), what="scan start")
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
    assert run("captain", "delete", allow=True).returncode, "nothing left to delete"
    print("Verified delete stops running work, removes the Captain, and keeps threads, transcripts, and worktrees.", flush=True)
finally:
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as c:
            c.execute("delete from watches")
    subprocess.run(["pkill", "-f", str(temp)], check=False)
