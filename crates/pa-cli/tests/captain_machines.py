"""Captain-started factory runs: headless host, idempotent dispatch, permission gate,
crew rules, writer worktrees, one-answer resume, and crash reconciliation.
Usage: python3 captain_machines.py PATH_TO_PA [TEMP_DIR]"""
import json, os, pathlib, signal, sqlite3, subprocess, sys, tempfile, time
from concurrent.futures import ThreadPoolExecutor

P = pathlib.Path
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
CHILDREN = {
    "sw-review-*": {"responses": [emit("finding", "No issues in parser"), "Emitted."]},
    "sw-check-*": {"responses": ["I could not verify the tree."]},
    "sw-check-*-a2": {"responses": [emit("verdict", "verified"), "Emitted."]},
}


FAUX = temp / "faux.json"


def script(*responses):
    # One fixed file: a worker that a watcher started with an older environment still reads
    # the current script, never a stale inline copy.
    draft = temp / "faux.json.tmp"
    draft.write_text(json.dumps({"responses": list(responses) or ["Captain received the report."], "children": CHILDREN}))
    os.replace(draft, FAUX)
    env["PRIME_AGENT_FAUX_SCRIPT"] = str(FAUX)


env = dict(os.environ, PA_CAPTAIN_HOME=str(hub), PRIME_AGENT_CODING_AGENT_DIR=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native),
           PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"), PA_CAPTAIN_NATIVE_SOCKET=f"/tmp/pa-cm-{os.getpid()}.sock",
           DO_NOT_TRACK="1")
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


def lead_cell(op, **args):
    """A scripted Captain turn step: run one act command from the Prime kernel."""
    cmd = ["env", f"PA_CAPTAIN_HOME={hub}", f"PA_CAPTAIN_NATIVE_HOME={native}", f"PA_CAPTAIN_ACTOR={pid}", binary, "thread", "captain", "act", op]
    code = ("import shlex, json\n"
            f"r = await bash('printf %s ' + shlex.quote({json.dumps(args)!r}) + ' | ' + {' '.join(cmd)!r} + ' 2>&1')\n"
            "print(r)")
    return {"content": [{"type": "toolCall", "name": "python_repl", "arguments": {"code": code}}]}


def lead_turn(text, *responses):
    # A crew event can queue a digest turn; let it finish so it cannot take this turn's script.
    wait(lambda: not db().execute("select 1 from jobs j join projects p on j.member=p.lead where j.status in ('queued','running')").fetchone(),
         what="idle Captain before " + text)
    script(*responses)
    jid = act("lead", project=pid, text=text)["job"]
    return wait(lambda: (lambda r: r if r[0] not in ("queued", "running") else None)(
        db().execute("select status,result from jobs where id=?", (jid,)).fetchone()), what="lead turn " + text)


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
    status, result = lead_turn("Review the parser.", lead_cell("delegate", project=pid, machine="check-sweep", task="Review the parser", request_id="sweep-1"), {"systemPrompt": True})
    assert status == "done", result
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
    gate = act("delegate", project=pid, machine="gate-check", task="Gate", request_id="gate-1")["id"]
    wait(lambda: member(gate)["state"] == "needs_you" and member(gate)["run_state"] == "paused", what="pause")
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
        f"shown = await bash('printf %s ' + shlex.quote({json.dumps(dict(project=pid, name='proj-build'))!r}) + ' | ' + {base + ' factory-show'!r})",
        "d = json.loads(shown.output)['digest']",
        f"r = await bash('printf %s ' + shlex.quote(json.dumps({{'project': {pid!r}, 'name': 'proj-build', 'digest': d}})) + ' | ' + {base + ' factory-approve 2>&1'!r})",
        "print(r)",
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

    # A design request is a Captain turn that may validate but never starts a run.
    wait(lambda: not db().execute("select 1 from jobs j join projects p on j.member=p.lead where j.status in ('queued','running')").fetchone(),
         what="idle Captain before design")
    script(lead_cell("factory-validate", project=pid, name="proj-build"),
           lead_cell("delegate", project=pid, machine="check-sweep", task="design-probe", request_id="design-probe"),
           {"systemPrompt": True})
    jid = act("factory-design", project=pid, request_id="d-1")["job"]
    assert act("factory-design", project=pid, request_id="d-2") == {"job": jid, "repeat": True}
    status, result = wait(lambda: (lambda r: r if r[0] not in ("queued", "running") else None)(
        db().execute("select status,result from jobs where id=?", (jid,)).fetchone()), what="design turn")
    assert status == "done", result
    for needle in ("Design request from the user", "rlm.factory.help()", "proj-build (project, approved)", "factory-validate", str(factories)):
        assert needle in result, needle
    assert not db().execute("select 1 from crew where title like '%design-probe%'").fetchone(), "a design turn started a run"
    lead = json.loads((hub / "threads" / (lead_id + ".json")).read_text())
    assert [body for role, body in lead["messages"] if role == "user"][-1] == "Design a factory for this project."
    assert "Use an action in this Captain" in refused("factory-design", actor=True)
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
    chat_script.write_text(json.dumps({"engine": "faux", "contextWindow": 1000000, "responses": [lead_cell("grant", project=pid, machine="slow-scan"), "Granted slow-scan."]}))
    chat_env = dict(env, PRIME_AGENT_CODING_AGENT_DIR=str(native), PRIME_AGENT_DAEMON_SOCKET=env["PA_CAPTAIN_NATIVE_SOCKET"],
                    PRIME_AGENT_FAUX_SCRIPT=str(chat_script))
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
        text = "You may run slow-scan freely."
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

    # Kill the worker and host mid-run, then restart: one loss report and no second start.
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
    wait(lambda: member(slow)["run_state"] == "lost", what="lost")
    for _ in range(3):
        stat()
    with db() as d:
        assert d.execute("select count(*) from reports where member=? and id like '%/end'", (slow,)).fetchone()[0] == 1
        assert d.execute("select status from jobs where member=?", (slow,)).fetchone()[0] == "needs_you"
    assert (control / "abandoned").exists() and [r["kind"] for r in journal(slow)].count("run_started") == 1
    rerun = subprocess.run([binary, "thread", "factory-host", str(control / "host.py"), member(slow)["ref"]], cwd=a,
                           env=dict(env, PRIME_AGENT_CODING_AGENT_DIR=str(native)), capture_output=True, text=True, timeout=120)
    assert rerun.returncode and "already started or abandoned" in rerun.stdout + rerun.stderr, rerun.stderr[-800:]
    assert [r["kind"] for r in journal(slow)].count("run_started") == 1
    print("Verified a killed host is marked lost once after restart and its run never starts twice.", flush=True)
finally:
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as d:
            d.execute("delete from watches")
    # The chat stand-in's daemon serves the short socket path; stop exactly that process.
    subprocess.run(["pkill", "-f", env["PA_CAPTAIN_NATIVE_SOCKET"]], check=False)
    P(env["PA_CAPTAIN_NATIVE_SOCKET"]).unlink(missing_ok=True)
    subprocess.run(["pkill", "-f", str(temp)], check=False)
