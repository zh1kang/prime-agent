"""Captain durability: WAL and durable status files, idle-checked recovery of a daemon turn from
its native session, a daemon-held factory run that survives its host with an answer outbox,
missing worktrees, pending writer-machine grants with one-step approval, Drive files with edit
history, archive, the reconcile command, and the cursor provider. A scripted daemon socket
stands in for the user's native daemon.
Usage: captain_durability.py PA TEMP"""
import json, os, pathlib, shutil, socket, sqlite3, subprocess, sys, threading, time, uuid

P = pathlib.Path
binary = str(P(sys.argv[1]).resolve())
temp = P(sys.argv[2]).resolve()
a = temp / "alpha"; a.mkdir()
hub = temp / "hub"; native = temp / "native"; native.mkdir()
sessions = native / "sessions"; sessions.mkdir()
mock = temp / "bin"; mock.mkdir()
(mock / "claude").write_text("""#!/usr/bin/env python3
import json,sys
sys.stdin.read()
print(json.dumps({"type":"result","session_id":"fa16e11d-0a87-47ae-a8d1-f6e3a12b806f","result":"Plan written.","is_error":False}))
""")
(mock / "claude").chmod(0o755)
d = native / "machines" / "builder-x"; d.mkdir(parents=True)
(d / "machine.toml").write_text('format = 1\nname = "builder-x"\nversion = 1\ndescription = "Build."\n'
                                '[params.task]\ntype = "text"\n'
                                '[states.build]\nentry = true\nkind = "python"\nrun = "nodes.py:build"\noutputs = { done = "text" }\n')
(d / "nodes.py").write_text("def build(inputs):\n    return {'done': 'built'}\n")
(d / "captain.toml").write_text('access = "write"\n')
(native / "settings.json").write_text(json.dumps(dict(factory=dict(enabled=True), onboardingShown=True, agentTraces=dict(enabled=False))))

# The macOS socket path limit is 104 bytes.
SOCK = f"/tmp/pa-cd-{os.getpid()}.sock"
env = dict(os.environ, PA_CAPTAIN_HOME=str(hub), PRIME_AGENT_CODING_AGENT_DIR=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native),
           PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"), PA_CAPTAIN_NATIVE_SOCKET=SOCK, PRIME_AGENT_SESSION_DIR=str(sessions),
           DO_NOT_TRACK="1", PATH=str(mock) + ":" + os.environ["PATH"])
for key in ("PRIME_AGENT_KERNEL_PYTHON", "PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION", "RLM_SESSION_ID"):
    env.pop(key, None)


class FakeDaemon:
    """Answers list, abort, and factory_activity like the native daemon, from `self.sessions`
    (session file -> row) and `self.runs` (run ID -> status)."""

    def __init__(self, path):
        self.sessions, self.runs, self.calls = {}, {}, []
        self.server = socket.socket(socket.AF_UNIX)
        self.server.bind(path)
        self.server.listen(16)
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                conn, _ = self.server.accept()
            except OSError:
                return
            threading.Thread(target=self.handle, args=(conn,), daemon=True).start()

    def handle(self, conn):
        with conn:
            conn.sendall(b'{"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":7}}\n')
            stream = conn.makefile("rb")
            for line in stream:
                frame = json.loads(line)
                command = frame["command"]
                self.calls.append(command)
                data, ok = {}, True
                if command["type"] == "list":
                    data = {"sessions": [dict(row, sessionFile=path) for path, row in self.sessions.items()]}
                elif command["type"] == "abort":
                    for row in self.sessions.values():
                        if row["activeSessionId"] == command["activeSessionId"]:
                            row["isStreaming"] = False
                elif command["type"] == "factory_activity":
                    run = self.runs.get(command.get("runId"))
                    if run is None:
                        ok = False
                    elif command["action"] == "resume":
                        run["state"] = "running"
                        data = {"notes_applied": 0}
                    elif command["action"] == "stop":
                        run["state"] = "stopped"
                    else:
                        data = dict(run)
                conn.sendall((json.dumps({"type": "response", "id": frame["id"], "success": ok, "data": data}) + "\n").encode())

    def close(self):
        self.server.close()


def run(*args, cwd=a, body=None, allow=False, extra=None):
    cp = subprocess.run([binary, "thread", *args], cwd=cwd, env=dict(env, **(extra or {})), input=body, text=True,
                        capture_output=True, timeout=120)
    if not allow and cp.returncode:
        raise RuntimeError(str(args) + " " + cp.stderr[-1500:])
    return cp


def act(op, actor=None, **args):
    return json.loads(run("captain", "act", op, body=json.dumps(args), extra={"PA_CAPTAIN_ACTOR": actor} if actor else None).stdout)


def refused(op, needle, actor=None, **args):
    cp = run("captain", "act", op, body=json.dumps(args), allow=True, extra={"PA_CAPTAIN_ACTOR": actor} if actor else None)
    assert cp.returncode and needle in cp.stderr and "Traceback" not in cp.stderr, (op, cp.stderr)
    return cp.stderr


def db():
    return sqlite3.connect(hub / "captains/projects.sqlite3")


def status():
    return json.loads(run("captain", "status").stdout)["projects"][0]


def member(project, mid):
    return next(m for m in project["crew"] if m["id"] == mid)


def wait(f, what, limit=60):
    for _ in range(int(limit / 0.2)):
        value = f()
        if value:
            return value
        time.sleep(0.2)
    raise RuntimeError("timeout: " + what)


def git(*args, cwd=a):
    return subprocess.check_output(["git", "-C", str(cwd), *args], text=True)


def message(role, text):
    return json.dumps({"type": "message", "message": {"role": role, "content": [{"type": "text", "text": text}]}})


daemon = FakeDaemon(SOCK)
pid = None
try:
    # The reconcile command creates nothing when no Captain exists.
    out = json.loads(run("captain", "reconcile").stdout)
    assert out == {"ok": True, "store": False} and not (hub / "captains/projects.sqlite3").exists(), out
    print("Verified reconcile is a no-op without a Captain store.", flush=True)

    git("init", "-q")
    git("-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "--allow-empty", "-qm", "base")
    pid = act("init", objective="Ship Alpha", provider="claude")["id"]
    wait(lambda: db().execute("select 1 from jobs where purpose='brief' and status='done'").fetchone(), "brief turn")
    assert db().execute("pragma journal_mode").fetchone()[0] == "wal"
    status()
    leftovers = [p.name for p in (hub / "captains").rglob("*.tmp")]
    assert not leftovers and json.loads((hub / "captains/roots.json").read_text()) == [str(a)], leftovers
    assert json.loads((hub / "captains/status" / (pid + ".json")).read_text())["project_id"] == pid
    print("Verified WAL mode and durable status and roots files with no temporary files left.", flush=True)

    # A crew worker died during a daemon turn. While the daemon still runs the turn, recovery waits;
    # once it is idle, the reply in the native session completes the job and clears the pending turn.
    tid = str(uuid.uuid4())
    session = sessions / (str(uuid.uuid4()) + ".jsonl")
    dispatch = "Check the parser"
    session.write_text("\n".join([
        json.dumps({"type": "session", "version": 3, "id": session.stem, "timestamp": "2026-11-22T00:00:00Z", "cwd": str(a)}),
        message("user", "older turn"), message("assistant", "older reply"),
        message("user", dispatch), message("assistant", "Parser checked: all good."),
    ]) + "\n")
    (hub / "threads" / (tid + ".json")).write_text(json.dumps(
        {"id": tid, "provider": "prime", "cwd": str(a), "native": str(session), "messages": [], "pending": dispatch}))
    jid = str(uuid.uuid4())
    with db() as c:
        c.execute("insert into crew(id,project,kind,ref,title,cwd,provider,state,detail,updated) values(?,?,'thread',?,?,?,'prime','running','',?)",
                  (tid, pid, tid, dispatch, str(a), time.time()))
        c.execute("insert into jobs(id,project,member,purpose,body,status,created,pid,initial_count,dispatch,adapter) "
                  "values(?,?,?,'delegate',?,'running',?,999999,0,?,?)", (jid, pid, tid, dispatch, time.time(), dispatch, tid))
    daemon.sessions[str(session)] = {"activeSessionId": "s-crew", "isStreaming": True}
    run("captain", "reconcile")
    assert db().execute("select status from jobs where id=?", (jid,)).fetchone()[0] == "running"
    daemon.sessions[str(session)]["isStreaming"] = False
    run("captain", "reconcile")
    assert db().execute("select status,result from jobs where id=?", (jid,)).fetchone() == ("done", "Parser checked: all good.")
    thread = json.loads((hub / "threads" / (tid + ".json")).read_text())
    assert thread["pending"] is None and thread["messages"] == [["user", dispatch], ["assistant", "Parser checked: all good."]], thread
    assert db().execute("select count(*) from reports where id=?", ("job/" + jid,)).fetchone()[0] == 1
    print("Verified idle-checked recovery of a finished daemon turn from its native session.", flush=True)

    # A Captain-started run whose host exited while the daemon session still holds it: reconcile
    # keeps it paused, not lost; an answer is committed, then delivered once through the daemon.
    mid = str(uuid.uuid4())
    run_session = sessions / (mid + ".jsonl")
    run_session.write_text(json.dumps({"type": "session", "version": 3, "id": mid, "timestamp": "2026-11-22T00:00:00Z", "cwd": str(a)}) + "\n")
    mjob = str(uuid.uuid4())
    control = hub / "captains/runs" / mid
    control.mkdir(parents=True)
    (control / "run.json").write_text(json.dumps({"pid": 999999, "run": "run-1"}))
    (control / "graph.json").write_text(json.dumps({"nodes": [{"id": "check", "status": "waiting", "child_id": "child-7"}]}))
    with db() as c:
        c.execute("insert into crew(id,project,kind,ref,title,cwd,provider,state,detail,updated) values(?,?,'factory',?,?,?,'prime','needs_you','Review needed at check',?)",
                  (mid, pid, str(run_session), "gate-check · Gate", str(a), time.time()))
        c.execute("insert into jobs(id,project,member,purpose,body,status,created,pid) values(?,?,?,'machine','{}','running',?,999999)",
                  (mjob, pid, mid, time.time()))
        c.execute("insert into machines(member,owner,state,children,job,session) values(?,'','paused','[]',?,?)", (mid, mjob, str(run_session)))
    daemon.sessions[str(run_session)] = {"activeSessionId": "s-run", "isStreaming": False}
    daemon.runs["run-1"] = {"state": "paused", "nodes": [], "events": []}
    run("captain", "reconcile")
    assert db().execute("select state,run_id from machines where member=?", (mid,)).fetchone() == ("paused", "run-1")
    row = member(status(), mid)
    assert row["run_id"] == "run-1" and row["host_session"] == str(run_session) and row["step_sessions"] == ["child-7"], row
    assert row["activity_state"] == "waiting" and row["needs_you"] is True, row
    answered = act("answer", project=pid, member=mid, text="Use the fixture", request_id="ans-1")
    assert act("answer", project=pid, member=mid, text="Use the fixture", request_id="ans-1")["repeat"] is True
    assert db().execute("select applied_at is not null,text from answers where member=?", (mid,)).fetchone() == (1, "Use the fixture")
    resumes = [c for c in daemon.calls if c.get("action") == "resume"]
    assert len(resumes) == 1 and resumes[0].get("note") == "Use the fixture", daemon.calls
    assert not list(control.glob("answer-*.json")), "the daemon run got its answer from the outbox"
    daemon.runs["run-1"] = {"state": "done", "nodes": [{"id": "check", "status": "done", "answerPreview": "passed"}],
                            "events": [{"kind": "milestone", "detail": "gate passed"}]}
    run("captain", "reconcile")
    assert db().execute("select state from machines where member=?", (mid,)).fetchone()[0] == "done"
    assert db().execute("select status,result from jobs where id=?", (mjob,)).fetchone() == ("done", "check: passed")
    print("Verified a daemon-held run survives its host, takes one answer from the outbox, and settles.", flush=True)

    # A writer worktree removed outside pa: no crash, a structured Needs you item, and a user-only recreate.
    wid = str(uuid.uuid4())
    tree = hub / "captains/worktrees" / wid
    base = git("rev-parse", "HEAD").strip()
    git("worktree", "add", "-q", "-b", "pa/" + wid, "--", str(tree), base)
    (hub / "threads" / (wid + ".json")).write_text(json.dumps(
        {"id": wid, "provider": "claude", "cwd": str(tree), "native": None, "messages": [], "pending": None}))
    with db() as c:
        c.execute("insert into crew(id,project,kind,ref,title,cwd,provider,state,detail,updated) values(?,?,'thread',?,?,?,'claude','done','',?)",
                  (wid, pid, wid, "Patch the parser", str(tree), time.time()))
        c.execute("insert into jobs(id,project,member,purpose,body,status,created,base_ref) values(?,?,?,'delegate','x','done',?,?)",
                  (str(uuid.uuid4()), pid, wid, time.time(), base))
    (tree / "work.txt").write_text("draft")
    assert member(status(), wid)["diff_added"] == 0
    shutil.rmtree(tree)
    project = status()
    assert member(project, wid)["worktree_missing"] is True
    assert any(i["kind"] == "worktree_missing" and i["member"] == wid for i in project["needs_you"]), project["needs_you"]
    refused("reply", "worktree-recreate", project=pid, member=wid, text="go on")
    refused("worktree-recreate", "Use an action", actor=pid, project=pid, member=wid)
    out = act("worktree-recreate", project=pid, member=wid)
    assert tree.is_dir() and out["branch"] == "pa/" + wid and git("branch", "--show-current", cwd=tree).strip() == "pa/" + wid
    assert member(status(), wid)["worktree_missing"] is False
    print("Verified a missing worktree becomes a Needs you item and worktree-recreate restores its branch.", flush=True)

    # A writer machine the Captain may not start becomes a pending grant; the user approves it in one step.
    err = refused("delegate", "Ask the user with NEEDS YOU: first.", actor=pid, project=pid, machine="builder-x", task="Build", request_id="b-1")
    gid = err.split("grant ", 1)[1].split(";", 1)[0]
    refused("delegate", "Needs you as grant " + gid, actor=pid, project=pid, machine="builder-x", task="Build", request_id="b-1")
    project = status()
    grants = [i for i in project["needs_you"] if i["kind"] == "pending_grant"]
    assert [g["grant_id"] for g in grants] == [gid] and project["pending_grants"][0]["machine"] == "builder-x", project["needs_you"]
    published = json.loads((hub / "captains/status" / (pid + ".json")).read_text())
    assert published["counts"]["needs_you"] == len(published["needs_you"]) == len(project["needs_you"]), published
    refused("grant-approve", "Use an action", actor=pid, project=pid, grant_id=gid)
    assert act("grant-decline", project=pid, grant_id=gid)["declined"] is True
    refused("delegate", "writes code", actor=pid, project=pid, machine="builder-x", task="Build", request_id="b-2")
    gid = db().execute("select id from pending_grants").fetchone()[0]
    started = act("grant-approve", project=pid, grant_id=gid, standing=True)
    assert started["job"] and not db().execute("select 1 from pending_grants").fetchone()
    assert json.loads(db().execute("select grants from projects").fetchone()[0]) == ["builder-x"]
    assert db().execute("select title from crew where id=?", (started["id"],)).fetchone()[0].startswith("builder-x")
    refused("grant-approve", "no longer pending", project=pid, grant_id=gid)
    wait(lambda: db().execute("select status from jobs where id=?", (started["job"],)).fetchone()[0] not in ("queued", "running"), "builder run")
    print("Verified pending grants, one-step approve with a standing grant, decline, and a unified Needs you count.", flush=True)

    # Drive files, edit history with restore, archive, and the cursor provider.
    act("edit", project=pid, field="rules", value="RULE-1 keep it short")
    act("edit", project=pid, field="rules", value="RULE-2 keep it shorter")
    act("edit", project=pid, field="plan", value="Step one")
    drive = a / ".prime/captain"
    assert (drive / "AGENTS.md").read_text() == "RULE-2 keep it shorter\n" and (drive / "NOTES.md").read_text() == "Step one\n"
    history = act("edit-history", project=pid, field="rules")["edits"]
    assert history[0]["restorable"] and history[0]["new_len"] == len("RULE-2 keep it shorter"), history
    refused("edit-restore", "Use an action", actor=pid, project=pid, edit_id=history[0]["id"])
    act("edit-restore", project=pid, edit_id=history[0]["id"])
    assert db().execute("select rules from projects").fetchone()[0] == "RULE-1 keep it short"
    assert (drive / "AGENTS.md").read_text() == "RULE-1 keep it short\n"
    act("archive", project=pid, member=wid)
    assert member(status(), wid)["archived"] is True
    act("unarchive", project=pid, member=wid)
    assert member(status(), wid)["archived"] is False
    assert act("crew-model", project=pid, member=wid, provider="cursor", model="")["provider"] == "cursor"
    refused("crew-model", "Choose Prime, Claude, Codex, Antigravity, or Cursor.", project=pid, member=wid, provider="nope", model="")
    print("Verified Drive files, edit history and restore, archive, and the cursor provider.", flush=True)

    # The Captain drafts a project factory on its own; the draft still waits for the user's approval.
    design = act("factory-design", actor=pid, project=pid, request_id="cd-1")["job"]
    wait(lambda: db().execute("select status from jobs where id=?", (design,)).fetchone()[0] not in ("queued", "running"), "design turn")
    print("Verified the Captain can request a factory design.", flush=True)
finally:
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as c:
            c.execute("delete from watches")
        if pid:
            run("captain", "stop", allow=True)
    daemon.close()
    P(SOCK).unlink(missing_ok=True)


# The native daemon hosts a Captain-started run: the run is live on the daemon's factory
# overview, outlives its factory host and the session's worker, and a daemon resume carries
# the user's reviewer note to the retried step.
live = temp / "live"; la = live / "alpha"; la.mkdir(parents=True)
lhub = live / "hub"; lnative = live / "native"
g = lnative / "machines" / "gate-check"; (g / "prompts").mkdir(parents=True)
(g / "machine.toml").write_text('format = 1\nname = "gate-check"\nversion = 1\ndescription = "Ask for review when its check fails."\n'
                                '[run]\nfailure_policy = "escalate"\n'
                                '[states.check]\nentry = true\nagent = { prompt = "prompts/check.md" }\noutputs = { verdict = "text" }\n')
(g / "prompts/check.md").write_text("Check the tree.")
(g / "captain.toml").write_text('access = "read"\n')
(lnative / "settings.json").write_text(json.dumps(dict(factory=dict(enabled=True), onboardingShown=True, agentTraces=dict(enabled=False))))
# Daemon steps all read one child script, at spawn. The first attempt's turn fails, so the run
# pauses for review; the retry, spawned after the answer, emits.
child = live / "child.json"
child.write_text(json.dumps({"engine": "faux", "responses": [
    {"content": [{"type": "text", "text": "I could not verify the tree."}], "stopReason": "error", "errorMessage": "The check failed."}]}))
emit = {"content": [{"type": "toolCall", "name": "python_repl", "arguments": {"code": "await rlm.factory.emit(verdict='verified')"}}]}
faux = live / "faux.json"
faux.write_text(json.dumps({"engine": "faux", "responses": ["Captain received the report."], "repeatLastResponse": True}))
LSOCK = f"/tmp/pa-cl-{os.getpid()}.sock"
lenv = dict(env, PA_CAPTAIN_HOME=str(lhub), PRIME_AGENT_CODING_AGENT_DIR=str(lhub), PA_CAPTAIN_NATIVE_HOME=str(lnative),
            PRIME_AGENT_DAEMON_SOCKET=str(live / "socket"), PA_CAPTAIN_NATIVE_SOCKET=LSOCK, PRIME_AGENT_FAUX_SCRIPT=str(faux),
            PRIME_AGENT_FAUX_CHILD_SCRIPT=str(child))
lenv.pop("PRIME_AGENT_SESSION_DIR", None)
git("init", "-q", cwd=la)
git("-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "--allow-empty", "-qm", "base", cwd=la)


def lact(op, **args):
    cp = subprocess.run([binary, "thread", "captain", "act", op], input=json.dumps(args), cwd=la, env=lenv, text=True,
                        capture_output=True, timeout=180)
    assert cp.returncode == 0, (op, cp.stderr[-1500:])
    return json.loads(cp.stdout)


def lreconcile():
    subprocess.run([binary, "thread", "captain", "reconcile"], cwd=la, env=lenv, check=True, capture_output=True, timeout=180)


def ldb():
    return sqlite3.connect(lhub / "captains/projects.sqlite3")


def lrun_state(mid):
    row = ldb().execute("select state from machines where member=?", (mid,)).fetchone()
    return row[0] if row else None


def ldaemon(command):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(60)
        s.connect(LSOCK)
        f = s.makefile("rw")
        f.readline()  # hello
        f.write(json.dumps({"type": "command", "id": "q", "protocol": {"name": "prime-agent.daemon", "version": 7}, "command": command}) + "\n")
        f.flush()
        while True:
            line = json.loads(f.readline())
            if line.get("id") == "q":
                return line


def lrows(session):
    """This run's rows on the daemon's factory overview, which the factory page and dock read."""
    reply = ldaemon({"type": "factory_activity", "activeSessionId": "", "action": "overview"})
    assert reply["success"], reply
    return [r for r in reply["data"]["runs"] if r["specId"] == "gate-check" and r["ownerId"] == session.stem]


def hosts():
    listed = subprocess.run(["ps", "-wwEAo", "pid=,command="], capture_output=True, text=True).stdout
    return [int(line.split(None, 1)[0]) for line in listed.splitlines()
            if " thread factory-host " in line and str(live) in line]


native_daemon = subprocess.Popen([binary, "--mode", "daemon", "--daemon-socket", LSOCK], cwd=live,
                                 env=dict(lenv, PRIME_AGENT_CODING_AGENT_DIR=str(lnative)),
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
lpid = None
try:
    wait(lambda: os.path.exists(LSOCK), "native daemon socket", 30)
    lpid = lact("init", objective="Gate the tree")["id"]
    gate = lact("delegate", project=lpid, machine="gate-check", task="Gate", request_id="lg-1")["id"]
    wait(lambda: lrun_state(gate) == "paused", "the review pause", 180)
    session = P(ldb().execute("select ref from crew where id=?", (gate,)).fetchone()[0].split("#")[0])
    rows = wait(lambda: lrows(session), "the live run on the daemon overview", 60)
    assert rows[0]["state"] == "paused", rows
    print("Verified a Captain-started run is hosted on the native daemon and shows on its overview.", flush=True)

    # The factory host dies; the daemon kernel still holds the run, so it stays paused, not lost.
    for host in wait(hosts, "the factory host process"):
        os.kill(host, 9)
    wait(lambda: not hosts(), "the factory host to exit")
    lreconcile()
    assert lrun_state(gate) == "paused" and lrows(session), (lrun_state(gate), lrows(session))
    # The session's worker ends too: reconcile reopens the session, its kernel rehydrates the run.
    worker = ldaemon({"type": "list", "includeClientOwned": True})["data"]["sessions"]
    owner = next(r for r in worker if os.path.realpath(r.get("sessionFile") or r.get("sessionPath") or "") == os.path.realpath(session))
    ldaemon({"type": "kill", "activeSessionId": owner.get("activeSessionId") or owner["id"]})
    lreconcile()
    assert lrun_state(gate) == "paused", lrun_state(gate)
    wait(lambda: [r for r in lrows(session) if r["state"] == "paused"], "the reopened run on the overview", 60)
    print("Verified the run survives its factory host and its worker, and reconcile reattaches it.", flush=True)

    # The answer goes out through the daemon's resume with the reviewer note.
    child.write_text(json.dumps({"engine": "faux", "responses": [emit, "Emitted."]}))
    lact("answer", project=lpid, member=gate, text="Use the fixture path", request_id="lg-ans")
    assert ldb().execute("select applied_at is not null from answers where member=?", (gate,)).fetchone() == (1,)
    def settled():
        lreconcile()
        return lrun_state(gate) == "done"
    wait(settled, "the resumed run to finish", 180)
    retried = [t for t in (q.read_text() for q in (lnative / "session-artifacts").rglob("sub-*/*.jsonl"))
               if "Reviewer note for this retry: Use the fixture path" in t]
    assert retried, "the reviewer note did not reach the retried step"
    assert not list((lhub / "captains/runs" / gate).glob("answer-*.json")), "the answer went through the daemon"
    assert not hosts(), hosts()
    print("Verified a daemon resume carries the reviewer note to the retried step and the run settles.", flush=True)
finally:
    if (lhub / "captains/projects.sqlite3").is_file():
        with ldb() as c:
            c.execute("delete from watches")
        if lpid:
            subprocess.run([binary, "thread", "captain", "stop"], cwd=la, env=lenv, capture_output=True, timeout=120)
    for host in hosts():
        os.kill(host, 9)
    native_daemon.terminate()
    native_daemon.wait(30)
    P(LSOCK).unlink(missing_ok=True)
print("ok")
