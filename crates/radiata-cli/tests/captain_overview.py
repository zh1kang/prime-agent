"""One daemon: a Captain-started factory run shows in the native daemon's factory_activity
overview (the factory page and the activity dock read it), stop removes it, and after resume a
new run shows again.
Usage: captain_overview.py PA TEMP"""
import json, os, pathlib, socket, sqlite3, subprocess, sys, time
P = pathlib.Path
binary = sys.argv[1]
temp = P(sys.argv[2]).resolve()
a = temp / "alpha"; a.mkdir(); hub = temp / "hub"; native = temp / "native"; native.mkdir()
d = native / "machines/slow-scan"; d.mkdir(parents=True)
(d / "machine.toml").write_text('format = 1\nname = "slow-scan"\nversion = 1\ndescription = "Scan slowly."\n'
                                '[states.scan]\nentry = true\nkind = "python"\nrun = "nodes.py:scan"\noutputs = { done = "text" }\n')
(d / "nodes.py").write_text("def scan(inputs):\n    import time\n    time.sleep(90)\n    return {'done': 'yes'}\n")
(d / "captain.toml").write_text('access = "read"\n')
(native / "settings.json").write_text(json.dumps(dict(factory=dict(enabled=True), onboardingShown=True, agentTraces=dict(enabled=False))))
# A short socket path: the macOS socket path limit is 104 bytes.
sock = f"/tmp/pa-co-{os.getpid()}.sock"
env = dict(os.environ, PA_CAPTAIN_HOME=str(hub), PRIME_AGENT_CODING_AGENT_DIR=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native),
           PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"), PA_CAPTAIN_NATIVE_SOCKET=sock, DO_NOT_TRACK="1",
           PRIME_AGENT_FAUX_SCRIPT=json.dumps({"responses": ["Captain received the report."] * 20}))
for key in ("PRIME_AGENT_KERNEL_PYTHON", "PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION"):
    env.pop(key, None)
subprocess.check_call(["git", "-C", str(a), "init", "-q"])
subprocess.check_call(["git", "-C", str(a), "-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "--allow-empty", "-qm", "base"])


def act(op, **args):
    cp = subprocess.run([binary, "thread", "captain", "act", op], input=json.dumps(args), cwd=a, env=env, text=True, capture_output=True, timeout=120)
    assert cp.returncode == 0, (op, cp.stderr)
    return json.loads(cp.stdout)


def db():
    return sqlite3.connect(hub / "captains/projects.sqlite3")


def run_state(mid):
    row = db().execute("select state from machines where member=?", (mid,)).fetchone()
    return row[0] if row else None


def wait(f, what, limit=120):
    for _ in range(int(limit / 0.25)):
        value = f()
        if value:
            return value
        time.sleep(0.25)
    raise RuntimeError("timeout: " + what)


def overview():
    """The factory_activity overview, as the TUI's factory page and dock request it."""
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(30)
        s.connect(sock)
        f = s.makefile("rw")
        f.readline()  # hello
        f.write(json.dumps({"type": "command", "id": "overview", "protocol": {"name": "prime-agent.daemon", "version": 7},
                            "command": {"type": "factory_activity", "activeSessionId": "", "action": "overview"}}) + "\n")
        f.flush()
        while True:
            line = json.loads(f.readline())
            if line.get("id") == "overview":
                assert line["success"], line
                return line["data"]["runs"]


daemon = subprocess.Popen([binary, "--mode", "daemon", "--daemon-socket", sock], cwd=temp,
                          env=dict(env, PRIME_AGENT_CODING_AGENT_DIR=str(native)),
                          stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
pid = None
try:
    wait(lambda: os.path.exists(sock), "native daemon socket", 30)
    pid = act("init", objective="Scan the tree")["id"]
    mid = act("delegate", project=pid, machine="slow-scan", task="scan", request_id="scan-1")["id"]
    wait(lambda: run_state(mid) == "running", "Captain run")
    session = P(db().execute("select ref from crew where id=?", (mid,)).fetchone()[0].split("#")[0])
    assert session.parent == native / "sessions", session
    rows = wait(lambda: [r for r in overview() if r["specId"] == "slow-scan"], "the run in the overview")
    assert rows[0]["state"] == "running" and rows[0]["ownerId"] == session.stem, rows
    print("Verified a Captain-started run shows in the native factory overview.", flush=True)

    act("stop", project=pid)
    wait(lambda: run_state(mid) == "stopped", "stopped run")
    wait(lambda: not [r for r in overview() if r["specId"] == "slow-scan"], "the stopped run leaves the overview")
    assert act("resume", project=pid)["state"] == "active"
    again = act("delegate", project=pid, machine="slow-scan", task="scan again", request_id="scan-2")["id"]
    wait(lambda: run_state(again) == "running", "second Captain run")
    rows = wait(lambda: [r for r in overview() if r["specId"] == "slow-scan"], "the second run in the overview")
    assert rows[0]["state"] == "running", rows
    print("Verified stop removes the run from the overview and resume starts work again.", flush=True)
finally:
    if pid:
        act("stop", project=pid)
        with db() as c:
            c.execute("delete from watches")  # the Captain watcher exits with no watches
    daemon.terminate()
    daemon.wait(20)
print("ok")
