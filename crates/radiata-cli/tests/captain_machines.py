"""Captain-started factory runs: headless host, idempotent dispatch, permission gate,
crew rules, writer worktrees, one-answer resume, and a run that outlives its host on the daemon.
Usage: python3 captain_machines.py PATH_TO_PA [TEMP_DIR]"""
import fcntl, json, os, pathlib, signal, sqlite3, subprocess, sys, tempfile, time, uuid
from concurrent.futures import ThreadPoolExecutor

P = pathlib.Path
sys.path.insert(0, str(P(__file__).resolve().parent))
from captain_daemon import retire  # noqa: E402
binary = str(P(sys.argv[1]).resolve())
temp = P(tempfile.mkdtemp(prefix="pa-captain-machines-", dir=sys.argv[2] if len(sys.argv) > 2 else "/tmp")).resolve()
a = temp / "alpha"; a.mkdir()
hub = temp / "hub"; native = temp / "native"; native.mkdir()


def machine(name, toml, files=(), access=None):
    d = native / "machines" / name
    (d / "prompts").mkdir(parents=True)
    (d / "machine.toml").write_text(f'format = 1\nname = "{name}"\nversion = 1\n' + toml)
    for path, text in files:
        (d / path).write_text(text)
    if access:
        (d / "captain.toml").write_text(f'access = "{access}"\n')


machine("check-sweep", '''description = "Review the tree and report one finding."
[params]
task = { type = "text", description = "What to review" }
[states.review]
entry = true
agent = { prompt = "prompts/review.md" }
outputs = { finding = "text" }
next = "publish"
[states.publish]
kind = "python"
run = "nodes.py:publish"
inputs = { finding = "review.finding" }
outputs = { report = "text" }
''', [("prompts/review.md", "Review: {{task}}"), ("nodes.py", "def publish(inputs):\n    return {'report': inputs['finding']}\n")], "read")
machine("gate-check", '''description = "Ask for review when its check fails."
[run]
failure_policy = "escalate"
[states.check]
entry = true
agent = { prompt = "prompts/check.md" }
outputs = { verdict = "text" }
''', [("prompts/check.md", "Check the tree.")], "read")
machine("builder-x", '''description = "Write a build marker into the checkout."
[states.build]
entry = true
kind = "python"
run = "nodes.py:build"
outputs = { built = "text" }
''', [("nodes.py", "def build(inputs):\n    from pathlib import Path\n    Path('built.txt').write_text('built')\n    return {'built': str(Path.cwd())}\n")])
machine("slow-scan", '''description = "Scan slowly."
[states.scan]
entry = true
kind = "python"
run = "nodes.py:scan"
outputs = { done = "text" }
''', [("nodes.py", "def scan(inputs):\n    import time\n    time.sleep(120)\n    return {'done': 'yes'}\n")], "read")
(native / "settings.json").write_text(json.dumps(dict(factory=dict(enabled=True), onboardingShown=True, agentTraces=dict(enabled=False))))

emit = lambda port, value: {"content": [{"type": "toolCall", "name": "python_repl", "arguments": {"code": f"await rlm.factory.emit({port}={value!r})"}}]}
FAILED_CHECK = {"content": [{"type": "text", "text": "I could not verify the tree."}], "stopReason": "error", "errorMessage": "The check failed."}
CHILD = temp / "child.json"


def child_script(*responses):
    """Factory steps run as daemon children, and every child reads this one script when it
    spawns, so each phase writes the script its next steps run."""
    draft = temp / "child.json.tmp"
    draft.write_text(json.dumps({"engine": "faux", "responses": list(responses)}))
    os.replace(draft, CHILD)


child_script(emit("finding", "No issues in parser"), "Emitted.")


FAUX = temp / "faux.json"


def faux_json(*responses):
    # "engine": "faux" runs the real agent engine in a daemon worker, so tool cells run. The worker
    # keeps its queue across turns, so later turns (digests) get the default reply.
    default = "Captain received the report."
    return json.dumps({"engine": "faux", "responses": [*responses, default], "repeatLastResponse": True})


def script(*responses):
    # One fixed file: a worker that a watcher started with an older environment still reads
    # the current script, never a stale inline copy.
    draft = temp / "faux.json.tmp"
    draft.write_text(faux_json(*responses))
    os.replace(draft, FAUX)
    env["PRIME_AGENT_FAUX_SCRIPT"] = str(FAUX)


def fresh_lead():
    """Captain turns run on the daemon, and its worker reads the faux script once. Retire the
    lead's worker so the next turn reads the script this test writes."""
    lead_id = db().execute("select lead from projects").fetchone()[0]
    retire(env["PA_CAPTAIN_NATIVE_SOCKET"], json.loads((hub / "threads" / (lead_id + ".json")).read_text())["native"])


def scoped(purpose, body, *responses):
    """Write a script for one Captain turn. Every Captain turn reads the same file, and a digest
    turn can start before or after the scripted turn. Each tool cell acts only when the running
    Captain job is the scripted turn, and its first cell sets the file back to the default
    reply, so no other turn replays the scripted actions."""
    fresh_lead()
    script(*scoped_responses(purpose, body, FAUX, faux_json(), *responses))


def scoped_responses(purpose, body, target, default, *responses):
    """The responses of one scripted Captain turn in the script file `target`, guarded as in
    scoped(); the first cell that acts writes `default` to `target`."""
    reset = f"_d = {str(target) + '.reset'!r}\nopen(_d, 'w').write({default!r})\nos.replace(_d, {str(target)!r})\n"
    out, first = [], True
    for r in responses:
        if isinstance(r, dict) and r.get("content", [{}])[0].get("name") == "python_repl":
            code = r["content"][0]["arguments"]["code"]
            guard = ("import os, sqlite3\n"
                     f"_job = sqlite3.connect({str(hub / 'captains/projects.sqlite3')!r}).execute("
                     "\"select j.purpose, j.body from jobs j join projects p on j.member=p.lead where j.status='running'\").fetchone()\n"
                     f"if _job is None or _job[0] != {purpose!r}{'' if body is None else f' or _job[1] != {body!r}'}:\n"
                     "    print('Not the scripted turn.')\n"
                     "else:\n")
            body_code = (reset if first else "") + code
            code = guard + "".join("    " + line + "\n" for line in body_code.splitlines())
            r = {"content": [{"type": "toolCall", "name": "python_repl", "arguments": {"code": code}}]}
            first = False
        out.append(r)
    assert not first, "a scoped script needs a tool cell"
    return out


env = dict(os.environ, PA_CAPTAIN_HOME=str(hub), PRIME_AGENT_CODING_AGENT_DIR=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native),
           PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"), PA_CAPTAIN_NATIVE_SOCKET=f"/tmp/pa-cm-{os.getpid()}.sock",
           PRIME_AGENT_FAUX_CHILD_SCRIPT=str(CHILD), DO_NOT_TRACK="1")
env.pop("PRIME_AGENT_KERNEL_PYTHON", None)
script()


def run(*args, body=None, allow=False, cwd=a):
    cp = subprocess.run([binary, "thread", *args], cwd=cwd, env=env, input=body, text=True, capture_output=True, timeout=120)
    if not allow and cp.returncode:
        raise RuntimeError(str(args) + " " + cp.stderr[-1500:])
    return cp


def act(op, **args): return json.loads(run("captain", "act", op, body=json.dumps(args)).stdout)
def stat(): return json.loads(run("captain", "status").stdout)
def db(): return sqlite3.connect(hub / "captains/projects.sqlite3")
def git(*args): return subprocess.check_output(["git", "-C", str(a), *args], text=True)
def member(mid): return next(m for m in stat()["projects"][0]["crew"] if m["id"] == mid)


def wait(f, limit=150, what=""):
    for _ in range(int(limit / 0.25)):
        res = f()
        if res:
            return res
        time.sleep(0.25)
    with db() as d:
        raise RuntimeError(f"timeout {what} " + str(d.execute("select c.title,c.state,c.detail,m.state from crew c left join machines m on m.member=c.id").fetchall()))


# An await on bash returns a running snapshot after one second. On a loaded host the act command
# takes longer, and the completion notice then asks the model for one more reply than the script
# has. Await the handle in the same cell until the command ends; that also withdraws the notice.
AWAIT_DONE = "while r.running:\n    r = await r.handle\n"


def lead_cell(op, **args):
    """A scripted Captain turn step: run one act command from the Prime kernel."""
    # The turn's own process holds its script inline. Processes that this cell starts (a factory
    # host, the queue watcher) read the shared file instead, so they never replay this turn.
    cmd = ["env", f"PA_CAPTAIN_HOME={hub}", f"PA_CAPTAIN_NATIVE_HOME={native}", f"PA_CAPTAIN_ACTOR={pid}",
           f"PRIME_AGENT_FAUX_SCRIPT={FAUX}", binary, "thread", "captain", "act", op]
    code = ("import shlex, json\n"
            f"r = await bash('printf %s ' + shlex.quote({json.dumps(args)!r}) + ' | ' + {' '.join(cmd)!r} + ' 2>&1')\n"
            + AWAIT_DONE + "print(r)")
    return {"content": [{"type": "toolCall", "name": "python_repl", "arguments": {"code": code}}]}


def lead_turn(text, *responses):
    # A crew event can queue a digest turn; let it finish so it cannot take this turn's script.
    wait(lambda: not db().execute("select 1 from jobs j join projects p on j.member=p.lead where j.status in ('queued','running')").fetchone(),
         what="idle Captain before " + text)
    scoped("lead", text, *responses)
    jid = act("lead", project=pid, text=text)["job"]
    return wait(lambda: (lambda r: r if r[0] not in ("queued", "running") else None)(
        db().execute("select status,result from jobs where id=?", (jid,)).fetchone()), what="lead turn " + text)


def hidden_context():
    """The Captain context of the last lead turn: a hidden row in the lead's native session,
    which the model reads and the chat does not show."""
    lead_id = db().execute("select lead from projects").fetchone()[0]
    path = json.loads((hub / "threads" / (lead_id + ".json")).read_text())["native"]
    found = []

    def visit(value):
        if isinstance(value, dict):
            if value.get("customType") == "hidden_context":
                found.append(value)
            for item in value.values():
                visit(item)
        elif isinstance(value, list):
            for item in value:
                visit(item)

    for line in P(path).read_text().splitlines():
        visit(json.loads(line))
    assert found, "the lead session has no hidden Captain context"
    assert found[-1].get("display") is False, found[-1]
    content = found[-1]["content"]
    return content if isinstance(content, str) else json.dumps(content)


def journal(mid):
    path = native / "session-artifacts" / mid / "captain" / "runs.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def ended(mid):
    with db() as d:
        return d.execute("select state from machines where member=? and state in ('done','failed','stopped','lost')", (mid,)).fetchone()


print(temp, flush=True)
try:
    git("init", "-q")
    git("-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "--allow-empty", "-qm", "base")
    pid = act("init", objective="Ship Alpha")["id"]
    wait(lambda: db().execute("select 1 from jobs where purpose='brief' and status='done'").fetchone(), what="brief")
    act("edit", project=pid, field="crew_rules", value="CREWRULE-7 indent with tabs")
    act("edit", project=pid, field="rules", value="CAPTAINRULE-9 keep the digest short")

    # The Captain chooses a read-only machine on its own; it runs with no chat open.
    # The faux model answers with its system prompt: the Captain context reaches the model there,
    # while the transcript keeps only the user's text.
    status, result = lead_turn("Review the parser.", lead_cell("delegate", project=pid, machine="check-sweep", task="Review the parser", request_id="sweep-1"), "Turn done.")
    assert status == "done", result
    result = hidden_context()
    lead = json.loads((hub / "threads" / (db().execute("select lead from projects").fetchone()[0] + ".json")).read_text())
    users = [body for role, body in lead["messages"] if role == "user"]
    assert users[-1] == "Review the parser.", users[-1][:300]
    assert not any("persistent Captain" in u for u in users), [u[:80] for u in users]
    for needle in ("persistent Captain", "PA_CAPTAIN_ACTOR=" + pid, "check-sweep (read-only): Review the tree", "builder-x (writes code)", "call grant(machine) in that turn", "cannot approve an ungranted writer machine", "answer(member,decision,text,request_id)", "CAPTAINRULE-9"):
        assert needle in result, needle
    sweep = db().execute("select id from crew where title like 'check-sweep%'").fetchone()[0]
    wait(lambda: ended(sweep), what="sweep end")
    row = member(sweep)
    assert row["run_state"] == "done" and row["state"] == "done" and row["progress"] == "2/2", row
    assert "No issues in parser" in row["detail"], row["detail"]
    assert [r["kind"] for r in journal(sweep)].count("run_started") == 1
    with db() as d:
        assert d.execute("select count(*) from reports where member=? and id like '%/end'", (sweep,)).fetchone()[0] == 1
        assert d.execute("select count(*) from jobs where purpose='digest'").fetchone()[0] >= 1
    print("Verified the Captain started a read-only machine and it ran headless to done.", flush=True)

    # Crew rules reach every agent step; the Captain's rules never do.
    texts = [p.read_text() for p in (native / "session-artifacts" / sweep).glob("sub-*/*.jsonl")]
    assert texts, "no factory agent session"
    assert any("CREWRULE-7" in t and "Review: Review the parser" in t for t in texts), "crew rules missing from the agent step"
    assert not any("CAPTAINRULE-9" in t for t in texts), "Captain rules leaked into a factory session"
    print("Verified crew rules reach factory agent steps and Captain rules do not.", flush=True)

    # One request ID gives one run, even when sent concurrently.
    def same(_): return act("delegate", project=pid, machine="check-sweep", task="Second sweep", request_id="sweep-2")
    with ThreadPoolExecutor(2) as pool:
        r1, r2 = pool.map(same, range(2))
    assert r1 == r2 and act("delegate", project=pid, machine="check-sweep", task="Second sweep", request_id="sweep-2") == r1
    wait(lambda: ended(r1["id"]), what="second sweep")
    with db() as d:
        assert d.execute("select count(*) from jobs where origin like 'request/%/sweep-2'").fetchone()[0] == 1
    assert [r["kind"] for r in journal(r1["id"])].count("run_started") == 1
    bad = run("captain", "act", "delegate", body=json.dumps(dict(project=pid, machine="check-sweep", params={"nope": 1}, task="x")), allow=True)
    assert bad.returncode and "no parameter nope" in bad.stderr, bad.stderr
    bad = run("captain", "act", "delegate", body=json.dumps(dict(project=pid, machine="missing", task="x")), allow=True)
    assert bad.returncode and "installed machine" in bad.stderr, bad.stderr
    print("Verified idempotent machine dispatch and catalog validation.", flush=True)

    # A writer machine needs the user's agreement unless the Captain's rules allow it.
    status, result = lead_turn("Build it.", lead_cell("delegate", project=pid, machine="builder-x", task="Build", request_id="build-1"),
                               "NEEDS YOU: builder-x writes code. May I start it?")
    assert status == "needs_you" and "NEEDS YOU" in result, (status, result)
    assert not db().execute("select 1 from crew where title like 'builder-x%'").fetchone()
    gate = subprocess.run([binary, "thread", "captain", "act", "delegate"], cwd=a, env=dict(env, PA_CAPTAIN_ACTOR=pid), text=True, capture_output=True,
                          input=json.dumps(dict(project=pid, machine="builder-x", task="Build", request_id="build-1")))
    assert gate.returncode and "builder-x writes code" in gate.stderr and "Ask the user with NEEDS YOU: first." in gate.stderr, gate.stderr
    print("Verified a writer machine without a rule gives NEEDS YOU and starts nothing.", flush=True)

    # On a turn started by a crew event, the Captain cannot approve a writer or grant one,
    # even when it claims approved true. The turn type comes from the job queue.
    lead_id = db().execute("select lead from projects where id=?", (pid,)).fetchone()[0]
    with db() as d:
        d.execute("insert into jobs(id,project,member,purpose,body,status,created,origin,pid) values('fake-digest',?,?,'digest','x','running',?,NULL,?)",
                  (pid, lead_id, time.time(), os.getpid()))
    try:
        as_captain = dict(env, PA_CAPTAIN_ACTOR=pid)
        forged = subprocess.run([binary, "thread", "captain", "act", "delegate"], cwd=a, env=as_captain, text=True, capture_output=True,
                                input=json.dumps(dict(project=pid, machine="builder-x", task="Build", request_id="build-forged", approved=True)))
        assert forged.returncode and "Only a turn that answers a user message can approve it" in forged.stderr, forged.stderr
        granted = subprocess.run([binary, "thread", "captain", "act", "grant"], cwd=a, env=as_captain, text=True, capture_output=True,
                                 input=json.dumps(dict(project=pid, machine="builder-x")))
        assert granted.returncode and "answers a user message" in granted.stderr, granted.stderr
    finally:
        with db() as d:
            d.execute("delete from jobs where id='fake-digest'")
    assert not db().execute("select 1 from crew where title like 'builder-x%'").fetchone()
    assert db().execute("select grants from projects where id=?", (pid,)).fetchone()[0] == "[]"
    print("Verified a crew-event turn cannot approve or grant a writer machine.", flush=True)
    status, result = lead_turn("You may run builder-x freely.",
                               lead_cell("edit", project=pid, field="rules", value="CAPTAINRULE-9 keep the digest short\nYou may run builder-x freely."),
                               lead_cell("grant", project=pid, machine="builder-x"),
                               lead_cell("delegate", project=pid, machine="builder-x", task="Build", request_id="build-2"),
                               "Rules updated. Started builder-x.")
    assert status == "done", result
    assert "You may run builder-x freely." in db().execute("select rules from projects").fetchone()[0]
    assert json.loads(db().execute("select grants from projects").fetchone()[0]) == ["builder-x"]
    assert db().execute("select actor from edits where field='grants'").fetchone()[0] == "captain"
    build = db().execute("select id,cwd from crew where title like 'builder-x%'").fetchone()
    assert build, "builder-x did not start"
    wait(lambda: ended(build[0]), what="build end")
    tree = P(build[1])
    assert member(build[0])["run_state"] == "done"
    assert tree != a and (tree / "built.txt").is_file() and not (a / "built.txt").exists()
    assert git("worktree", "list").count(str(tree)) == 1
    print("Verified the rule allowed builder-x and the writer ran in its own worktree.", flush=True)

    # A review pause is "needs you"; one answer resumes it once.
    child_script(FAILED_CHECK)
    gate = act("delegate", project=pid, machine="gate-check", task="Gate", request_id="gate-1")["id"]
    wait(lambda: member(gate)["state"] == "needs_you" and member(gate)["run_state"] == "paused", what="pause")
    child_script(emit("verdict", "verified"), "Emitted.")
    assert "Review needed at check" in member(gate)["detail"]
    with ThreadPoolExecutor(2) as pool:
        answers = list(pool.map(lambda i: run("captain", "act", "answer", body=json.dumps(dict(project=pid, member=gate, text="Retry", request_id=f"ans-{i}")), allow=True), range(2)))
    assert sorted(cp.returncode for cp in answers) == [0, 1], [cp.stderr for cp in answers]
    assert "already has an answer" in next(cp.stderr for cp in answers if cp.returncode)
    winner = next(i for i, cp in enumerate(answers) if cp.returncode == 0)
    assert act("answer", project=pid, member=gate, text="Retry", request_id=f"ans-{winner}")["repeat"] is True
    wait(lambda: ended(gate), what="gate end")
    late = run("captain", "act", "answer", body=json.dumps(dict(project=pid, member=gate, text="Again", request_id="ans-late")), allow=True)
    assert late.returncode and "not waiting" in late.stderr
    assert [r["kind"] for r in journal(gate)].count("resumed") == 1
    assert member(gate)["run_state"] == "done"
    retried = [t for t in (q.read_text() for q in (native / "session-artifacts" / gate).glob("sub-*/*.jsonl"))
               if "Reviewer note for this retry: Retry" in t]
    assert retried, "the answer text did not reach the retried step"
    print("Verified a review pause shows as needs you, one answer resumes it with its note, and a second does not.", flush=True)
    child_script(FAILED_CHECK)
    halt = act("delegate", project=pid, machine="gate-check", task="Gate again", request_id="gate-2")["id"]
    wait(lambda: member(halt)["run_state"] == "paused", what="second pause")
    act("answer", project=pid, member=halt, decision="stop", text="Not needed", request_id="halt-1")
    wait(lambda: ended(halt), what="stop")
    assert member(halt)["run_state"] == "stopped" and member(halt)["state"] == "done", member(halt)
    print("Verified a paused run stops on a stop answer.", flush=True)

    # Project factories: the Captain writes a machine in the project; the user approves one version.
    factories = a / ".prime" / "machines"
    pf = factories / "proj-build"
    (pf / "nodes").mkdir(parents=True)
    (pf / "machine.toml").write_text('format = 1\nname = "proj-build"\nversion = 1\ndescription = "Write a marker."\n'
                                     '[states.build]\nentry = true\nkind = "python"\nrun = "nodes/build.py:build"\noutputs = { built = "text" }\n')
    (pf / "nodes/build.py").write_text("def build(inputs):\n    from pathlib import Path\n    Path('proj.txt').write_text('built')\n    return {'built': 'yes'}\n")
    # The Captain could write this itself; a project factory ignores it and always writes code.
    (pf / "captain.toml").write_text('access = "read"\n')
    clash = factories / "check-sweep"
    clash.mkdir()
    (clash / "machine.toml").write_text((native / "machines/check-sweep/machine.toml").read_text())

    def factory(name):
        return next(f for f in stat()["projects"][0]["factories"] if f["name"] == name)

    def refused(op, actor=False, **args):
        cp = subprocess.run([binary, "thread", "captain", "act", op], cwd=a, env=dict(env, PA_CAPTAIN_ACTOR=pid) if actor else env,
                            text=True, capture_output=True, input=json.dumps(dict(project=pid, **args)))
        assert cp.returncode, (op, cp.stdout)
        return cp.stderr

    row = factory("proj-build")
    assert row["state"] == "draft" and len(row["digest"]) == 64 and not row["errors"], row
    row = factory("check-sweep")
    assert row["state"] == "invalid" and "library machine named check-sweep" in row["errors"][0], row
    err = refused("delegate", machine="check-sweep", task="x", request_id="clash-1")
    assert "Both the library machine check-sweep" in err and str(clash) in err, err
    import shutil
    shutil.rmtree(clash)
    # A path under __pycache__ is outside the approved version.
    cache = factories / "cache-trick"
    (cache / "__pycache__").mkdir(parents=True)
    (cache / "__pycache__/p.md").write_text("Hidden prompt")
    (cache / "machine.toml").write_text('format = 1\nname = "cache-trick"\nversion = 1\n[states.a]\nentry = true\n'
                                        'agent = { prompt = "__pycache__/p.md" }\noutputs = { x = "text" }\n')
    row = factory("cache-trick")
    assert row["state"] == "invalid" and any("__pycache__" in e for e in row["errors"]), row
    shutil.rmtree(cache)
    assert [f["name"] for f in stat()["projects"][0]["factories"]] == ["proj-build"]
    shown = act("factory-show", project=pid, name="proj-build")
    assert shown["state"] == "draft" and shown["graph"]["states"][0]["id"] == "build" and shown["live"] is False, shown
    err = refused("delegate", machine="proj-build", task="Build", request_id="pf-0")
    assert "NEEDS YOU: proj-build is a draft" in err, err
    assert not db().execute("select 1 from crew where title like 'proj-build%'").fetchone()
    print("Verified the catalog lists project factories, refuses a library name, and refuses a draft run.", flush=True)

    # Only a turn that answers a user message approves; the Captain never deletes.
    with db() as d:
        d.execute("insert into jobs(id,project,member,purpose,body,status,created,origin,pid) values('fake-digest-2',?,?,'digest','x','running',?,NULL,?)",
                  (pid, lead_id, time.time(), os.getpid()))
    try:
        err = refused("factory-approve", actor=True, name="proj-build", digest=shown["digest"])
        assert "Only a turn that answers a user message can approve it" in err, err
    finally:
        with db() as d:
            d.execute("delete from jobs where id='fake-digest-2'")
    assert "Only the user deletes" in refused("factory-delete", actor=True, name="proj-build")
    # A crew thread or factory host is never the user.
    crew = dict(env, PA_CAPTAIN_ACTOR="crew:" + pid)
    for op, extra in (("factory-approve", dict(name="proj-build", digest=shown["digest"])), ("factory-delete", dict(name="proj-build")),
                      ("grant", dict(machine="builder-x")), ("factory-design", {})):
        cp = subprocess.run([binary, "thread", "captain", "act", op], cwd=a, env=crew, text=True, capture_output=True,
                            input=json.dumps(dict(project=pid, **extra)))
        assert cp.returncode and "crew thread cannot" in cp.stderr, (op, cp.stderr)
    cp = subprocess.run([binary, "thread", "captain", "act", "factory-show"], cwd=a, env=crew, text=True, capture_output=True,
                        input=json.dumps(dict(project=pid, name="proj-build")))
    assert cp.returncode == 0 and json.loads(cp.stdout)["state"] == "draft", cp.stderr
    assert factory("proj-build")["state"] == "draft"
    print("Verified a crew-event turn cannot approve a project factory and the Captain cannot delete one.", flush=True)

    approved = act("factory-approve", project=pid, name="proj-build", digest=shown["digest"])
    assert approved["state"] == "approved" and approved["by"] == "user"
    assert factory("proj-build")["state"] == "approved"
    assert act("choices", project=pid)["project_factories"] == ["proj-build"]
    err = refused("delegate", machine="proj-build", task="Build", write=False, request_id="pf-1")
    assert "proj-build writes code" in err, err
    built = act("delegate", project=pid, machine="proj-build", task="Build", request_id="pf-2")["id"]
    wait(lambda: ended(built), what="project factory run")
    assert member(built)["run_state"] == "done", member(built)
    tree = P(db().execute("select cwd from crew where id=?", (built,)).fetchone()[0])
    assert tree != a and (tree / "proj.txt").is_file() and not (a / "proj.txt").exists()
    assert (hub / "captains/runs" / built / "machine/proj-build/machine.toml").is_file(), "the run did not use the approved copy"
    print("Verified an approved project factory runs as a writer from a copy of the approved files.", flush=True)

    # The chart builder composes ONE end-to-end factory from ordered stages; the user approves it.
    import tomllib
    stages = ["plan", "implement", "validate", "review-triad"]
    refused("factory-compose", actor=True, name="pipeline", stages=stages)
    assert not (factories / "pipeline").exists()
    assert "Start the factory with plan or implement" in refused("factory-compose", name="pipeline", stages=["validate"])
    composed = act("factory-compose", project=pid, name="pipeline", stages=stages,
                   models={"implement": {"model": "", "thinking": ""}})
    assert composed["state"] == "draft" and not composed["errors"] and "needs approval" in composed["notice"], composed
    cf = factories / "pipeline"
    subprocess.check_call([binary, "factory", "validate", str(cf)], env=env, stdout=subprocess.DEVNULL)
    spec = tomllib.loads((cf / "machine.toml").read_text())
    st = spec["states"]
    assert st["plan"]["entry"] is True and st["plan"]["next"] == "implement", st["plan"]
    assert st["implement"]["next"] == "validate-run", st["implement"]
    assert st["validate-run"]["route"] == {"on": "passed", "true": "review-triad-start", "false": "implement"}, st["validate-run"]
    assert st["review-triad-verdict"]["route"] == {"on": "verdict", "approve": "done", "rework": "implement"}, st["review-triad-verdict"]
    lock = tomllib.loads((cf / "modules.lock").read_text())
    assert set(lock["modules"]) == {"validate", "review-triad"} and lock["modules"]["review-triad"]["version"] == 1, lock
    assert (cf / "modules/review-triad/prompts/bugs.md").is_file() and not (cf / "modules/review-triad/module.toml").exists()
    err = refused("delegate", machine="pipeline", task="Build", request_id="pipe-0")
    assert "NEEDS YOU: pipeline is a draft" in err, err
    assert "exists" in refused("factory-compose", name="pipeline", stages=stages)
    # Chart edits after onboarding: add and remove a stage; each version needs approval again.
    act("factory-approve", project=pid, name="pipeline", digest=composed["digest"])
    added = act("factory-add-stage", project=pid, name="pipeline", stage="consensus")
    assert added["state"] == "changed", added
    st = tomllib.loads((cf / "machine.toml").read_text())["states"]
    assert st["plan"]["next"] == "consensus-start" and st["consensus-start"]["next"] == ["consensus-a", "consensus-b"], st
    assert st["consensus-vote"]["route"] == {"on": "verdict", "approve": "implement", "revise": "plan"}, st
    removed = act("factory-remove-stage", project=pid, name="pipeline", stage="validate")
    st = tomllib.loads((cf / "machine.toml").read_text())["states"]
    assert removed["state"] == "changed" and "validate-run" not in st and st["implement"]["next"] == "review-triad-start", st["implement"]
    assert st["review-triad-start"]["next"] == ["review-triad-correctness", "review-triad-bugs", "review-triad-robustness"], st
    assert "validate" not in tomllib.loads((cf / "modules.lock").read_text())["modules"]
    assert "not built from the chart" in refused("factory-add-stage", name="proj-build", stage="validate")
    act("factory-delete", project=pid, name="pipeline")
    print("Verified factory-compose writes one valid end-to-end factory with its rework loop and modules.lock.", flush=True)

    # An edit after approval makes it a draft again until the user approves the new version.
    with (pf / "nodes/build.py").open("a") as f:
        f.write("# changed\n")
    assert factory("proj-build")["state"] == "changed"
    err = refused("delegate", machine="proj-build", task="Build", request_id="pf-3")
    assert "NEEDS YOU: proj-build changed since the user approved it" in err, err
    assert "changed since this review" in refused("factory-approve", name="proj-build", digest=shown["digest"])
    fresh = act("factory-show", project=pid, name="proj-build")["digest"]
    status, result = lead_turn("Approve proj-build.", lead_cell("factory-approve", project=pid, name="proj-build", digest=fresh), "Approved.")
    assert status == "done" and factory("proj-build")["state"] == "approved", (status, result)
    assert db().execute("select actor from factory_approvals where name='proj-build'").fetchone()[0] == "captain"
    # A Captain cannot change a factory and approve the change in the same turn.
    cmd = ["env", f"PA_CAPTAIN_HOME={hub}", f"PA_CAPTAIN_NATIVE_HOME={native}", f"PA_CAPTAIN_ACTOR={pid}", binary, "thread", "captain", "act"]
    base = " ".join(cmd)
    code = "\n".join([
        "import json, shlex",
        f"open({str(pf / 'nodes/build.py')!r}, 'a').write('# sneak\\n')",
        f"r = await bash('printf %s ' + shlex.quote({json.dumps(dict(project=pid, name='proj-build'))!r}) + ' | ' + {base + ' factory-show'!r})",
        AWAIT_DONE + "d = json.loads(r.output)['digest']",
        f"r = await bash('printf %s ' + shlex.quote(json.dumps({{'project': {pid!r}, 'name': 'proj-build', 'digest': d}})) + ' | ' + {base + ' factory-approve 2>&1'!r})",
        AWAIT_DONE + "print(r)",
    ])
    sneak = {"content": [{"type": "toolCall", "name": "python_repl", "arguments": {"code": code}}]}
    status, result = lead_turn("Tidy proj-build.", sneak, "Done.")
    assert factory("proj-build")["state"] == "changed", (status, result, json.loads((hub / "threads" / (lead_id + ".json")).read_text())["messages"][-4:])
    fresh = act("factory-show", project=pid, name="proj-build")["digest"]
    act("factory-approve", project=pid, name="proj-build", digest=fresh)
    print("Verified an edit after approval refuses runs until the user approves the new version in a user turn.", flush=True)

    # A live run blocks delete.
    with db() as d:
        d.execute("insert into jobs(id,project,member,purpose,body,status,created,origin,pid) values('fake-run',?,'fake-member','machine',?,'running',?,NULL,?)",
                  (pid, json.dumps({"name": "proj-build", "source": "project"}), time.time(), os.getpid()))
    try:
        assert "is live" in refused("factory-delete", name="proj-build")
        assert act("factory-show", project=pid, name="proj-build")["live"] is True
    finally:
        with db() as d:
            d.execute("delete from jobs where id='fake-run'")
    assert pf.is_dir()
    print("Verified delete is refused while a run of the factory is live.", flush=True)

    # Prompt-driven model changes and pre-approved edits, while a run of the factory is live.
    pm = factories / "proj-models"
    (pm / "prompts").mkdir(parents=True)
    (pm / "prompts/a.md").write_text("Work on: {{task}}")
    (pm / "machine.toml").write_text(
        'format = 1\nname = "proj-models"\nversion = 1\ndescription = "Model overlay fixture."\n'
        '[params]\ntask = { type = "text", description = "What to do" }\n'
        '[roles]\ndefault = { model = "anthropic/claude-opus-5-5", thinking = "low" }\n'
        '[roles.reviewer]\nmodel = "openai-codex/gpt-5.3-codex"\nthinking = "medium"\n'
        '[states.draft]\nentry = true\nagent = { prompt = "prompts/a.md" }\noutputs = { x = "text" }\nnext = "check"\n'
        '[states.check]\ninputs = { prev = "draft.x" }\noutputs = { x = "text" }\n'
        '[states.check.agent]\nrole = "reviewer"\nprompt = "prompts/a.md"\n')
    argon = "antigravity/gemini-4-argon"
    lead_env = dict(env, PA_CAPTAIN_ACTOR=pid)

    def as_actor(op, actor_env, **args):
        return subprocess.run([binary, "thread", "captain", "act", op], cwd=a, env=actor_env, text=True,
                              capture_output=True, input=json.dumps(dict(project=pid, **args)))

    held = {}

    def turn(purpose):
        # A worker holds its job lock while the turn runs; recovery settles a running job without one.
        jid = str(uuid.uuid4())
        held[jid] = (hub / "captains" / f"job-{jid}.lock").open("a+b")
        fcntl.flock(held[jid], fcntl.LOCK_EX)
        with db() as d:
            d.execute("insert into jobs(id,project,member,purpose,body,status,created,origin,pid) values(?,?,?,?,'x','running',?,NULL,?)",
                      (jid, pid, lead_id, purpose, time.time(), os.getpid()))
        return jid

    def end_turn(jid):
        with db() as d:
            d.execute("delete from jobs where id=?", (jid,))
        held.pop(jid).close()

    first = act("factory-show", project=pid, name="proj-models")
    assert first["state"] == "draft" and first["model_overrides"] == {"roles": {}, "states": {}}, first
    approved = act("factory-approve", project=pid, name="proj-models", digest=first["digest"])
    snap = P(approved["snapshot"])
    assert snap.name == first["digest"] and (snap / "machine.toml").read_text() == (pm / "machine.toml").read_text(), approved
    assert act("factory-approve", project=pid, name="proj-models", digest=first["digest"])["snapshot"] == str(snap)
    subprocess.check_call([binary, "factory", "validate", str(snap)], env=env, stdout=subprocess.DEVNULL)
    print("Verified approval stores a digest-addressed snapshot that is reused while it verifies.", flush=True)

    with db() as d:
        d.execute("insert into jobs(id,project,member,purpose,body,status,created,origin,pid) values('fake-run-2',?,'fake-member','machine',?,'running',?,NULL,?)",
                  (pid, json.dumps({"name": "proj-models", "source": "project"}), time.time(), os.getpid()))
    try:
        for purpose in ("digest", "brief", "design"):
            jid = turn(purpose)
            try:
                for op, extra in (("factory-set-model", dict(all=True, model=argon, thinking="high")),
                                  ("factory-preapprove", {}), ("factory-clear-model-override", dict(all=True))):
                    cp = as_actor(op, lead_env, name="proj-models", **extra)
                    assert cp.returncode and "turn that answers a user message" in cp.stderr, (purpose, op, cp.stderr)
            finally:
                end_turn(jid)
        crew_env = dict(env, PA_CAPTAIN_ACTOR="crew:" + pid)
        for op in ("factory-set-model", "factory-preapprove", "factory-add-stage"):
            cp = as_actor(op, crew_env, name="proj-models", all=True, model=argon, stage="validate")
            assert cp.returncode and "crew thread cannot" in cp.stderr, (op, cp.stderr)
        before = (pm / "machine.toml").read_text()
        jid = turn("lead")
        try:
            cp = as_actor("factory-set-model", lead_env, name="proj-models", all=True, model=argon, thinking="high")
            assert cp.returncode == 0, cp.stderr
            row = json.loads(cp.stdout)
            cp = as_actor("factory-set-model", lead_env, name="proj-models", all=True, model=argon, thinking="high")
            assert cp.returncode == 0, cp.stderr
            again = json.loads(cp.stdout)
        finally:
            end_turn(jid)
        assert (pm / "machine.toml").read_text() == before, "an overlay edit wrote machine.toml"
        assert row["state"] == "approved" and row["live"] is True and row["digest"] == first["digest"], row
        assert row["model_overrides"]["states"] == {s: {"model": argon, "thinking": "high"} for s in ("draft", "check")}, row
        assert set(row["model_overrides"]["roles"]) == {"default", "reviewer"}, row
        assert row.get("graph"), (row.get("graph_error"), row.get("overlay_error"))
        for node in row["graph"]["states"]:
            assert (node["subagent"]["model"], node["subagent"]["thinking"]) == (argon, "high"), node
        for sid in ("draft", "check"):
            assert (row["state_details"][sid]["model"], row["state_details"][sid]["thinking"]) == (argon, "high"), row["state_details"][sid]
        assert row["default_role"] == {"model": argon, "thinking": "high"}, row
        assert again["graph"] == row["graph"] and again["model_overrides"] == row["model_overrides"]
        assert factory("proj-models")["state"] == "approved" and factory("proj-models")["model_overrides"] == row["model_overrides"]
        assert act("factory-list", project=pid)["factories"][-1]["model_overrides"] == row["model_overrides"]
        assert db().execute("select actor from edits where field='factory-model proj-models' order by stamp desc").fetchone()[0] == "captain-lead"
        one = act("factory-set-model", project=pid, name="proj-models", role="reviewer", model="anthropic/claude-opus-5-5")
        assert one["model_overrides"]["roles"]["reviewer"] == {"model": "anthropic/claude-opus-5-5"} and one["state"] == "approved", one
        assert "agent state" in run("captain", "act", "factory-set-model", body=json.dumps(dict(
            project=pid, name="proj-models", state="nope", model=argon)), allow=True).stderr
        print("Verified a lead turn sets every FSM model at once while a run is live, keeping the approval.", flush=True)

        # A run that starts now uses the overlay; a later change does not reach it. The fake live
        # row would hold the parallel slot, so it steps aside for the real run.
        with db() as d:
            d.execute("update jobs set status='done' where id='fake-run-2'")
        # Its steps name a real model, so the run is stopped once its copy is staged.
        started = act("delegate", project=pid, machine="proj-models", task="Overlay run", request_id="pm-1", title="Overlay run")["id"]
        staged = hub / "captains/runs" / started / "machine/proj-models/machine.toml"
        wait(lambda: staged.is_file() and member(started)["run_state"] == "running", what="staged overlay copy")
        copy = tomllib.loads(staged.read_text())
        assert copy["states"]["draft"]["agent"]["model"] == argon and copy["states"]["check"]["agent"]["thinking"] == "high", copy
        cleared = act("factory-clear-model-override", project=pid, name="proj-models", all=True)
        assert cleared["model_overrides"] == {"roles": {}, "states": {}} and cleared["state"] == "approved", cleared
        check = next(n for n in cleared["graph"]["states"] if n["id"] == "check")
        assert (check["subagent"]["model"], check["subagent"]["thinking"]) == ("openai-codex/gpt-5.3-codex", "medium"), check
        assert tomllib.loads(staged.read_text()) == copy
        act("stop", project=pid)
        wait(lambda: ended(started), what="stopped overlay run")
        act("resume", project=pid)
        print("Verified a run starts on the overlay of its start and later overrides leave it untouched.", flush=True)
        with db() as d:
            d.execute("update jobs set status='running' where id='fake-run-2'")

        # Without a pre-approval a lead turn cannot approve its own edit; with one it can.
        jid = turn("lead")
        try:
            with (pm / "prompts/a.md").open("a") as f:
                f.write("Be brief.\n")
            fresh = json.loads(as_actor("factory-show", lead_env, name="proj-models").stdout)["digest"]
            cp = as_actor("factory-approve", lead_env, name="proj-models", digest=fresh)
            assert cp.returncode and "changed during this turn" in cp.stderr, cp.stderr
            cp = as_actor("factory-preapprove", lead_env, name="proj-models")
            assert cp.returncode == 0 and json.loads(cp.stdout)["preapproved"] is True, cp.stderr
            cp = as_actor("factory-approve", lead_env, name="proj-models", digest=fresh)
            assert cp.returncode == 0 and json.loads(cp.stdout)["state"] == "approved", cp.stderr
            cp = as_actor("factory-preapprove", lead_env, name="proj-models", allow=False)
            assert cp.returncode == 0 and json.loads(cp.stdout)["preapproved"] is False, cp.stderr
        finally:
            end_turn(jid)
        assert factory("proj-models")["state"] == "approved" and factory("proj-models")["digest"] == fresh
        actors = [r[0] for r in db().execute("select actor from edits where field in ('factory proj-models','factory-preapprove proj-models') order by stamp")]
        assert actors[-3:] == ["captain-lead", "captain-lead", "captain-lead"], actors
        assert db().execute("select lead_approve_factories from projects where id=?", (pid,)).fetchone()[0] == "{}"
        print("Verified a pre-approved lead turn approves its own edit while a run is live and records captain-lead.", flush=True)
    finally:
        with db() as d:
            d.execute("delete from jobs where id='fake-run-2'")
    act("factory-delete", project=pid, name="proj-models")

    # A design request is a Captain turn that may validate but never starts a run.
    wait(lambda: not db().execute("select 1 from jobs j join projects p on j.member=p.lead where j.status in ('queued','running')").fetchone(),
         what="idle Captain before design")
    scoped("design", None, lead_cell("factory-validate", project=pid, name="proj-build"),
           lead_cell("delegate", project=pid, machine="check-sweep", task="design-probe", request_id="design-probe"),
           "Turn done.")
    jid = act("factory-design", project=pid, request_id="d-1")["job"]
    assert act("factory-design", project=pid, request_id="d-2") == {"job": jid, "repeat": True}
    status, result = wait(lambda: (lambda r: r if r[0] not in ("queued", "running") else None)(
        db().execute("select status,result from jobs where id=?", (jid,)).fetchone()), what="design turn")
    assert status == "done", result
    result = hidden_context()
    for needle in ("Design request from the user", "rlm.factory.help()", "proj-build (project, approved)", "factory-validate", str(factories)):
        assert needle in result, needle
    assert not db().execute("select 1 from crew where title like '%design-probe%'").fetchone(), "a design turn started a run"
    lead = json.loads((hub / "threads" / (lead_id + ".json")).read_text())
    assert [body for role, body in lead["messages"] if role == "user"][-1] == "Design a factory for this project."
    # The Captain may draft a factory on its own, but a design turn does not start another.
    with db() as d:
        d.execute("insert into jobs(id,project,member,purpose,body,status,created,origin,pid) values('fake-design',?,?,'design','x','running',?,NULL,?)",
                  (pid, lead_id, time.time(), os.getpid()))
    try:
        assert "already designing" in refused("factory-design", actor=True)
    finally:
        with db() as d:
            d.execute("delete from jobs where id='fake-design'")
    act("factory-delete", project=pid, name="proj-build")
    assert not pf.exists() and not stat()["projects"][0]["factories"]
    assert not db().execute("select 1 from factory_approvals").fetchone()
    print("Verified a design turn starts no run and the user can delete a project factory.", flush=True)

    # A message typed in the open Captain chat. An ACP client holds the lead session the way pa's
    # chat does; the lead job runs through that chat's daemon, the Captain context arrives as a
    # hidden row, and a grant given in that turn is accepted because the turn is a user message.
    wait(lambda: not db().execute("select 1 from jobs j join projects p on j.member=p.lead where j.status in ('queued','running')").fetchone(),
         what="idle Captain before the chat turn")
    lead_session = json.loads((hub / "threads" / (lead_id + ".json")).read_text())["native"]
    chat_script = temp / "chat-faux.json"
    text = "You may run slow-scan freely."
    # The chat's daemon also runs a digest turn that starts meanwhile: scope the grant to this turn.
    chat_default = json.dumps({"engine": "faux", "contextWindow": 1000000, "responses": ["Captain received the report."]})
    chat_script.write_text(json.dumps({"engine": "faux", "contextWindow": 1000000, "responses": scoped_responses(
        "lead", text, chat_script, chat_default, lead_cell("grant", project=pid, machine="slow-scan"), "Granted slow-scan.")}))
    chat_env = dict(env, PRIME_AGENT_CODING_AGENT_DIR=str(native), PRIME_AGENT_DAEMON_SOCKET=env["PA_CAPTAIN_NATIVE_SOCKET"],
                    PRIME_AGENT_FAUX_SCRIPT=str(chat_script))
    # The chat starts the lead's daemon worker, which then reads the chat's script.
    fresh_lead()
    chat = subprocess.Popen([binary, "--mode", "acp", "--resume", lead_session, "--cwd", str(a)], cwd=a, env=chat_env,
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(temp / "chat.err", "w"), text=True)
    try:
        def acp(i, method, params):
            chat.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n")
            chat.stdin.flush()
            for line in chat.stdout:
                message = json.loads(line)
                if message.get("id") == i:
                    assert "error" not in message, message
                    return message["result"]
            raise RuntimeError("the chat stand-in stopped: " + (temp / "chat.err").read_text()[-1500:])
        acp(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        acp(2, "session/new", {"cwd": str(a), "mcpServers": []})
        size = P(lead_session).stat().st_size
        jid = act("lead", project=pid, text=text)["job"]
        status, result = wait(lambda: (lambda r: r if r[0] not in ("queued", "running") else None)(
            db().execute("select status,result from jobs where id=?", (jid,)).fetchone()), what="chat turn")
        assert status == "done" and "Granted slow-scan." in result, (result, P(lead_session).read_text()[size:][-3000:])
        assert "slow-scan" in json.loads(db().execute("select grants from projects").fetchone()[0])
        with open(lead_session) as f:
            f.seek(size)
            rows = [json.loads(line) for line in f if line.strip()]
        kinds = [(r.get("type"), r.get("customType") or (r.get("message") or {}).get("role")) for r in rows]
        hidden = next(i for i, r in enumerate(rows) if r.get("type") == "custom_message")
        assert rows[hidden]["display"] is False and "persistent Captain" in json.dumps(rows[hidden]["content"]), kinds
        assert "PA_CAPTAIN_ACTOR=" + pid in json.dumps(rows[hidden]["content"])
        user = next(i for i, r in enumerate(rows) if r.get("type") == "message" and r["message"]["role"] == "user")
        assert hidden < user and text in json.dumps(rows[user]["message"]["content"]), kinds
        assert "persistent Captain" not in json.dumps(rows[user]), rows[user]
        thread = json.loads((hub / "threads" / (lead_id + ".json")).read_text())["messages"]
        assert thread[-2] == ["user", text], thread[-2:]
    finally:
        chat.stdin.close()
        chat.wait(timeout=30)
    print("Verified a message in the open Captain chat runs as a Captain turn with hidden context and its grant counts.", flush=True)

    # Kill the job worker and the factory host mid-run: the daemon kernel keeps the run, so it
    # stays live, never starts twice, and still stops on the user's stop.
    slow = act("delegate", project=pid, machine="slow-scan", task="Scan", request_id="slow-1")["id"]
    control = hub / "captains" / "runs" / slow
    wait(lambda: (control / "run.json").exists() and "run" in json.loads((control / "run.json").read_text()), what="slow start")
    worker = db().execute("select pid from jobs where member=?", (slow,)).fetchone()[0]
    host = subprocess.check_output(["pgrep", "-f", f"factory-host {control}"], text=True).split()
    os.killpg(os.getpgid(worker), signal.SIGKILL)
    for p in host:
        try:
            os.kill(int(p), signal.SIGKILL)
        except ProcessLookupError:
            pass
    for _ in range(3):
        stat()
    assert member(slow)["run_state"] == "running", member(slow)
    assert not (control / "abandoned").exists()
    with db() as d:
        assert d.execute("select count(*) from reports where member=? and id like '%/end'", (slow,)).fetchone()[0] == 0
    rerun = subprocess.run([binary, "thread", "factory-host", str(control / "host.py"), member(slow)["ref"]], cwd=a,
                           env=dict(env, PRIME_AGENT_CODING_AGENT_DIR=str(native), PRIME_AGENT_DAEMON_SOCKET=env["PA_CAPTAIN_NATIVE_SOCKET"]),
                           capture_output=True, text=True, timeout=120)
    assert rerun.returncode and "already started or abandoned" in rerun.stdout + rerun.stderr, rerun.stderr[-800:]
    assert [r["kind"] for r in journal(slow)].count("run_started") == 1
    act("stop", project=pid)
    wait(lambda: (stat(), ended(slow))[1], what="stopped scan")
    assert member(slow)["run_state"] == "stopped", member(slow)
    with db() as d:
        assert d.execute("select count(*) from reports where member=? and id like '%/end'", (slow,)).fetchone()[0] == 1
    print("Verified a run outlives its killed host and worker on the daemon, never starts twice, and stops.", flush=True)
finally:
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as d:
            d.execute("delete from watches")
    # The chat stand-in's daemon serves the short socket path; stop exactly that process.
    subprocess.run(["pkill", "-f", env["PA_CAPTAIN_NATIVE_SOCKET"]], check=False)
    P(env["PA_CAPTAIN_NATIVE_SOCKET"]).unlink(missing_ok=True)
    subprocess.run(["pkill", "-f", str(temp)], check=False)
