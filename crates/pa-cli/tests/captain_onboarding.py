"""Captain-owned rules, model choice, promotion, and demotion at the CLI boundary.

A scripted `claude` executable plays both the Captain and its crew. As the
Captain it calls the `act` command from its prompt, like a real model would.
"""

import json, os, pathlib, sqlite3, subprocess, sys, time

P = pathlib.Path
binary = str(P(sys.argv[1]).resolve())
temp = P(sys.argv[2]).resolve()
a = temp / "alpha"
b = temp / "beta"
a.mkdir()
b.mkdir()
hub = temp / "hub"
native = temp / "native"
native.mkdir()
# An API key makes the built-in Anthropic catalog available to `pa model list`.
(native / "models.json").write_text(
    json.dumps({"providers": {"anthropic": {"apiKey": "test", "models": [{"id": "claude-test-1"}]}}})
)
machine = hub / "machines" / "plan-check"
machine.mkdir(parents=True)
(machine / "machine.toml").write_text(
    """format = 1
name = "plan-check"
version = 1
description = "Check a plan"
[params.task]
type = "text"
[states.check]
entry = true
kind = "python"
run = "check.py:check"
outputs = { result = "text" }
"""
)
(machine / "check.py").write_text("def check(inputs):\n    return {'result': 'ok'}\n")
mock = temp / "bin"
mock.mkdir()
script = mock / "claude"
script.write_text(
    r"""#!/usr/bin/env python3
import json, os, pathlib, re, shlex, subprocess, sys, uuid
body = sys.stdin.read()
with pathlib.Path(os.environ["CAPTURE"]).open("a") as f:
    f.write(json.dumps({"cwd": os.getcwd(), "args": sys.argv[1:], "body": body}) + "\n")

def act(op, **obj):
    cmd = shlex.split(re.search(r"Use the installed command (.+?) OP, sending", body).group(1))
    obj["project"] = re.search(r"Include project=([0-9a-f-]+) in", body).group(1)
    subprocess.run(cmd + [op], input=json.dumps(obj), text=True, check=True, capture_output=True)

def field(label):
    return re.search(re.escape(label) + r": (.*)\n", body).group(1)

reply = "crew done"
if "persistent Captain" in body:
    reply = "ok"
    if "This is your first turn" in body:
        act("edit", field="plan", value="1. Write the parser")
        act("edit", field="rules", value="CAPTAIN-ONLY: ask before deleting files.")
        act("edit", field="crew_rules", value="Run unit tests before reporting.")
        act("edit", field="digest", value="Needs you\n- nothing\nNew\n- plan written\nIn progress\n- none")
        reply = "Plan written."
    elif "From now on" in body:
        rules = field("Crew rules (every crew thread reads these each turn)")
        act("edit", field="crew_rules", value=rules + " Run the e2e suite before calling a PR ready.")
        reply = "Added to crew rules: run the e2e suite before calling a PR ready."
    elif "Crew event:" in body:
        act("edit", field="digest", value="Needs you\n- none\nNew\n- review-parser finished\nIn progress\n- none")
        reply = "Digest updated."
args = sys.argv[1:]
session = args[args.index("--resume") + 1] if "--resume" in args else str(uuid.uuid4())
print(json.dumps({"type": "result", "session_id": session, "result": reply, "is_error": False}))
"""
)
script.chmod(0o755)
env = dict(
    os.environ,
    PA_CAPTAIN_HOME=str(hub),
    PRIME_AGENT_CODING_AGENT_DIR=str(hub),
    PA_CAPTAIN_NATIVE_HOME=str(native),
    PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"),
    DO_NOT_TRACK="1",
    CAPTURE=str(temp / "capture"),
    PATH=str(mock) + ":" + os.environ["PATH"],
)
for key in ("PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SELECTION", "PA_CAPTAIN_SESSION", "PA_THREAD_MODEL"):
    env.pop(key, None)


def run(*args, cwd=a, body=None, allow=False, extra=None):
    cp = subprocess.run(
        [binary, "thread", *args],
        cwd=cwd,
        env={**env, **(extra or {})},
        input=body,
        text=True,
        capture_output=True,
        timeout=90,
    )
    if not allow and cp.returncode:
        raise RuntimeError(str(args) + " " + cp.stderr[-750:])
    return cp


def act(op, cwd=a, extra=None, **args):
    return json.loads(
        run("captain", "act", op, cwd=cwd, body=json.dumps(args), extra=extra).stdout
    )


def fails(op, cwd=a, extra=None, **args):
    cp = run("captain", "act", op, cwd=cwd, body=json.dumps(args), allow=True, extra=extra)
    assert cp.returncode, (op, args, cp.stdout)
    return cp.stderr


def db():
    return sqlite3.connect(hub / "captains/projects.sqlite3")


def project(pid):
    c = db()
    c.row_factory = sqlite3.Row
    return dict(c.execute("select * from projects where id=?", (pid,)).fetchone())


def wait(f, what):
    for _ in range(400):
        res = f()
        if res:
            return res
        time.sleep(0.15)
    raise RuntimeError(
        "timeout: " + what + " " + str(db().execute("select purpose,status,result from jobs").fetchall())
    )


def captures():
    path = temp / "capture"
    return [json.loads(l) for l in path.read_text().splitlines()] if path.exists() else []


def idle(pid):
    return not db().execute(
        "select 1 from jobs where project=? and status in ('queued','running')", (pid,)
    ).fetchone()


try:
    # Onboarding in an empty Git project without commits: brief, model, and worker limit.
    subprocess.check_call(["git", "init", "-q", str(a)])
    assert "Unknown claude model" in fails(
        "init", brief="Ship a parser", provider="claude", model="not-a-model"
    )
    assert not (hub / "captains/roots.json").exists() or a.as_posix() not in (
        hub / "captains/roots.json"
    ).read_text()
    p = act(
        "init",
        brief="Ship a parser for the config format.\nStart with the grammar.",
        provider="claude",
        model="claude-opus-5-5",
        parallel=3,
    )
    pid = p["id"]
    assert project(pid)["parallel"] == 3 and project(pid)["model"] == "claude-opus-5-5"
    assert str(a.resolve()) in json.loads((hub / "captains/roots.json").read_text())

    # The first Captain turn writes plan, rules, crew rules, and digest itself.
    wait(lambda: project(pid)["digest"] and idle(pid), "first turn")
    first = project(pid)
    assert first["plan"] == "1. Write the parser", first
    assert first["rules"].startswith("CAPTAIN-ONLY") and first["crew_rules"].startswith("Run unit tests")
    lead_call = next(c for c in captures() if "This is your first turn" in c["body"])
    assert lead_call["args"][lead_call["args"].index("--model") + 1] == "claude-opus-5-5"
    assert "Start with the grammar." in lead_call["body"]
    edits = db().execute("select field,actor from edits where project=?", (pid,)).fetchall()
    assert {f for f, who in edits if who == "captain"} >= {"plan", "rules", "crew_rules", "digest"}

    # A standing rule stated in chat lands in crew rules with a one-line confirmation.
    job = act("lead", project=pid, text="From now on, run the e2e suite before calling a PR ready.")
    wait(lambda: idle(pid), "rule turn")
    assert "crew rules" in db().execute("select result from jobs where id=?", (job["job"],)).fetchone()[0]
    assert project(pid)["crew_rules"].endswith("Run the e2e suite before calling a PR ready.")

    # Crew prompts carry crew rules and never the Captain's own rules.
    before = project(pid)["digest"]
    worker = act(
        "delegate", project=pid, provider="claude", task="review-parser", write=False, request_id="review-parser"
    )
    wait(lambda: project(pid)["digest"] != before and idle(pid), "digest after crew report")
    crew_call = next(c for c in captures() if "review-parser" in c["body"] and "persistent Captain" not in c["body"])
    assert "Run the e2e suite before calling a PR ready." in crew_call["body"]
    assert "CAPTAIN-ONLY" not in crew_call["body"]
    assert "--model" not in crew_call["args"]
    assert "review-parser finished" in project(pid)["digest"]
    # Transcripts show what the user typed or assigned; the Captain and crew wrappers stay hidden.
    crew_history = json.loads((hub / "threads" / (worker["id"] + ".json")).read_text())["messages"]
    assert crew_history[0] == ["user", "review-parser"], crew_history
    lead_history = json.loads((hub / "threads" / (project(pid)["lead"] + ".json")).read_text())["messages"]
    users = [body for role, body in lead_history if role == "user"]
    assert users[0].startswith("Brief: Ship a parser") and users[1].startswith("From now on"), users
    assert any(u.startswith("Crew update: ") for u in users), users
    assert not any("persistent Captain" in u or "Crew rules" in u for u in users), users

    # The user changes the model and thinking level; the Captain cannot.
    actor = {"PA_CAPTAIN_ACTOR": pid}
    assert "Unknown Captain setting" in fails(
        "edit", project=pid, field="model", value="claude-sonnet-4-6", extra=actor
    )
    assert "Unknown Captain setting" in fails(
        "edit", project=pid, field="thinking", value="high", extra=actor
    )
    act("edit", project=pid, field="model", provider="claude", value="claude-sonnet-4-6")
    assert project(pid)["model"] == "claude-sonnet-4-6"
    act("edit", project=pid, field="thinking", value="high")
    assert project(pid)["thinking"] == "high"
    assert db().execute(
        "select actor from edits where project=? and field='model'", (pid,)
    ).fetchone()[0] == "user"
    before_thinking = len(captures())
    act("lead", project=pid, text="Check thinking flag")
    wait(lambda: len(captures()) > before_thinking and idle(pid), "lead turn with thinking")
    thinking_call = captures()[-1]
    assert "--effort" in thinking_call["args"]
    assert thinking_call["args"][thinking_call["args"].index("--effort") + 1] == "high"

    # The optional starting factory becomes a drafted run with the brief as its task.
    # pa opens the returned target itself: the factory session with the run drafted.
    target = act("factory", project=pid, machine="plan-check")["handoff"]
    assert target["action"] == "resume" and pathlib.Path(target["value"]).is_file(), target
    draft = target["draft"]
    assert "rlm.factory.run('plan-check', params={'task': 'Ship a parser" in draft, draft

    # Promote an existing thread with history; its history stays and grows.
    tid = run("new", "claude", cwd=b).stdout.strip()
    run(tid, "send-stdin", cwd=b, body="Earlier discussion about the beta API")
    other = run("new", "claude", cwd=b).stdout.strip()
    # "Design a factory for this project" queues the design request after the brief turn.
    q = act("promote", cwd=b, ref=tid, brief="Finish the beta API", crew=[other], design=True)
    assert q["lead"] == tid and q["design_job"]
    wait(lambda: project(q["id"])["plan"] and idle(q["id"]), "promoted first turn")
    history = json.loads((hub / "threads" / (tid + ".json")).read_text())["messages"]
    assert history[0] == ["user", "Earlier discussion about the beta API"], history[:2]
    assert len(history) == 6 and history[2] == ["user", "Brief: Finish the beta API"], history
    assert history[4] == ["user", "Design a factory for this project."], history
    jobs = db().execute("select purpose,status from jobs where project=? and member=? order by created", (q["id"], tid)).fetchall()
    assert jobs == [("brief", "done"), ("design", "done")], jobs
    design_call = next(c for c in captures() if "Design request from the user" in c["body"])
    assert "rlm.factory.help()" in design_call["body"] and "do not start it" in design_call["body"]
    assert not db().execute("select 1 from crew where project=? and kind='factory'", (q["id"],)).fetchone()
    assert db().execute("select project from crew where id=?", (other,)).fetchone()[0] == q["id"]
    assert "already has a Captain" in fails("promote", cwd=b, ref=other, brief="x")
    assert "Captain cannot join" in fails("adopt", cwd=b, project=q["id"], ref=tid)

    # Demote keeps the thread and returns crew to the thread list.
    out = act("demote", project=q["id"], cwd=b)
    assert out["lead"] == tid and out["released"] == 1
    assert not db().execute("select 1 from projects where id=?", (q["id"],)).fetchone()
    assert not db().execute("select 1 from crew where id=?", (other,)).fetchone()
    assert run(tid, "context", cwd=b).returncode == 0
    assert str(b.resolve()) not in json.loads((hub / "captains/roots.json").read_text())
    print("Verified onboarding, Captain-written rules and digest, crew rule isolation, model choice, factory draft, promote, and demote.")
finally:
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as c:
            c.execute("delete from watches")
