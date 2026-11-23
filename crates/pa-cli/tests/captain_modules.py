"""Shared factory modules and project specialists: vendoring, validation, modules.lock, module
and specialist drift, re-approval, crew delegation with a specialist, path safety, and actor rules.
Usage: captain_modules.py PA TEMP"""
import atexit, json, os, pathlib, sqlite3, subprocess, sys, time, tomllib
exe = sys.argv[1]
r = pathlib.Path(sys.argv[2]).resolve()
a = r / "alpha"; a.mkdir(); agent = r / "agent"; agent.mkdir()
mock = r / "bin"; mock.mkdir()
call_log = r / "claude-calls.jsonl"
(mock / "claude").write_text(f"""#!/usr/bin/env python3
import json, sys
stdin = sys.stdin.read()
with open({str(call_log)!r}, "a") as fh:
    fh.write(json.dumps({{"args": sys.argv[1:], "stdin": stdin}}) + "\\n")
print(json.dumps({{"type": "result", "session_id": "fa16e11d-0a87-47ae-a8d1-f6e3a12b806f", "result": "crew ok", "is_error": False}}))
""")
(mock / "claude").chmod(0o755)
# Prime turns start a daemon on this socket; stop it when the test ends.
atexit.register(subprocess.run, ["pkill", "-f", str(r / "d.sock")], check=False)
env = dict(
    os.environ,
    PRIME_AGENT_CODING_AGENT_DIR=str(agent),
    PA_CAPTAIN_HOME=str(agent),
    PA_CAPTAIN_NATIVE_HOME=str(agent),
    PRIME_AGENT_DAEMON_SOCKET=str(r / "d.sock"),
    PA_CAPTAIN_NATIVE_SOCKET=str(r / "d.sock"),
    PRIME_AGENT_FAUX_SCRIPT=json.dumps({"responses": ["Captain received the brief."] * 8}),
    PATH=str(mock) + ":" + os.environ["PATH"],
    DO_NOT_TRACK="1",
)
for key in ("PRIME_AGENT_KERNEL_PYTHON", "PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION"):
    env.pop(key, None)
subprocess.check_call(["git", "-C", str(a), "init", "-q"])
subprocess.check_call(["git", "-C", str(a), "-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "--allow-empty", "-qm", "base"])
pf = a / ".prime/machines/ship"
(pf / "prompts").mkdir(parents=True)
(pf / "machine.toml").write_text(
    # A header and a key inside a multi-line string are text, not TOML structure.
    'format = 1\nname = "ship"\nversion = 1\ndescription = """Build, review, ship.\n[states.build]\nnext = "nowhere"\n"""\n\n'
    '[roles]\ncoder = { model = "anthropic/claude-sonnet-4-6" }\n\n'
    '[states.build]\nentry = true\nmax_entries = 4\nagent = { role = "coder", prompt = "prompts/build.md" } # a comment with }\n'
    'outputs = { summary = "text" }\nnext = "ship"\n\n'
    '[states.ship]\nagent = { role = "coder", prompt = "prompts/ship.md" }\n'
    'inputs = { summary = "build.summary" }\noutputs = { done = "text" }\nnext = "done"\n\n'
    '[states.done]\nkind = "python"\nrun = "nodes/done.py:done"\ninputs = { done = "ship.done" }\noutputs = { report = "text" }\n'
)
(pf / "nodes").mkdir()
(pf / "nodes/done.py").write_text("def done(inputs):\n    return {'report': inputs['done']}\n")
(pf / "prompts/build.md").write_text("Build the change.")
(pf / "prompts/ship.md").write_text("Ship {{summary}}.")


def run(op, actor=None, **d):
    e = dict(env, PA_CAPTAIN_ACTOR=actor) if actor else env
    return subprocess.run([exe, "thread", "captain", "act", op], input=json.dumps(d), text=True, env=e, cwd=a, capture_output=True)


def act(op, **d):
    cp = run(op, **d)
    if cp.returncode:
        raise RuntimeError(f"{op}: {cp.stderr[-1500:]}")
    return json.loads(cp.stdout)


def refused(op, needle, actor=None, **d):
    cp = run(op, actor=actor, **d)
    assert cp.returncode and needle in cp.stderr, (op, needle, cp.stdout, cp.stderr)


def status():
    return json.loads(subprocess.check_output([exe, "thread", "captain", "status"], text=True, env=env, cwd=a))


def factory(name="ship"):
    return next(f for f in status()["projects"][0]["factories"] if f["name"] == name)


def validate(path):
    subprocess.check_call([exe, "factory", "validate", str(path)], env=env, stdout=subprocess.DEVNULL)


pid = act("init", objective="Ship a parser")["id"]
db_path = agent / "captains/projects.sqlite3"


def wait_job(jid):
    for _ in range(300):
        with sqlite3.connect(db_path) as conn:
            row = conn.execute("select status,result from jobs where id=?", (jid,)).fetchone()
        if row and row[0] not in ("queued", "running"):
            return row
        time.sleep(0.2)
    raise RuntimeError(f"timeout waiting for job {jid}")


wait_job(sqlite3.connect(db_path).execute("select id from jobs where purpose='brief'").fetchone()[0])

# The library lists the bundled modules; a user module overrides one by name.
names = {m["name"]: m for m in act("factory-modules", project=pid)["modules"]}
assert {"review-triad", "sanitize-diff", "validate"} <= set(names), names
assert names["review-triad"]["source"] == "bundled" and names["review-triad"]["version"] == 1, names
assert names["review-triad"]["entry"] == ["correctness", "bugs", "robustness"], names

act("factory-approve", project=pid, name="ship", digest=factory()["digest"])
assert factory()["state"] == "approved"

# Vendoring: files copied, states merged with the alias prefix, wired, lock written, valid.
added = act("factory-add-module", project=pid, name="ship", module="review-triad", alias="review",
            after="build", wire={"subject": "build.summary", "approve": "ship", "rework": "build"})
assert added["state"] == "changed" and "needs approval" in added["notice"], added
manifest = tomllib.loads((pf / "machine.toml").read_text())
assert {"review-correctness", "review-bugs", "review-robustness", "review-verdict"} <= set(manifest["states"]), manifest
assert manifest["states"]["build"]["next"] == ["review-correctness", "review-bugs", "review-robustness"], manifest
assert manifest["states"]["review-verdict"]["route"] == {"on": "verdict", "approve": "ship", "rework": "build"}
assert manifest["states"]["review-verdict"]["run"] == "modules/review/nodes/verdict.py:merge"
assert {"reviewer-correctness", "reviewer-bugs", "reviewer-robustness"} <= set(manifest["roles"]), manifest
assert all(manifest["roles"][r]["model"] for r in ("reviewer-correctness", "reviewer-bugs", "reviewer-robustness")), manifest
assert (pf / "modules/review/prompts/bugs.md").is_file() and not (pf / "modules/review/module.toml").exists()
lock = tomllib.loads((pf / "modules.lock").read_text())
assert lock["modules"]["review"]["module"] == "review-triad" and lock["modules"]["review"]["version"] == 1, lock
assert len(lock["modules"]["review"]["digest"]) == 64, lock
assert sorted(lock["modules"]["review"]["roles"]) == ["reviewer-bugs", "reviewer-correctness", "reviewer-robustness"] and lock["modules"]["review"]["after"] == ["build"], lock
validate(pf)
act("factory-add-module", project=pid, name="ship", module="validate", alias="checks", after="ship",
    wire={"pass": "done", "fail": "build"})
act("factory-add-module", project=pid, name="ship", module="sanitize-diff", alias="clean", after="done",
    wire={"diff": "build.summary"})
validate(pf)
row = factory()
assert row["state"] == "changed" and row["drift"] == "", row
refused("factory-add-module", "already vendors", project=pid, name="ship", module="validate", alias="checks", wire={"pass": "done", "fail": "build"})
refused("factory-add-module", "Wire", project=pid, name="ship", module="review-triad", alias="r2", wire={})
act("factory-approve", project=pid, name="ship", digest=row["digest"])
show = act("factory-show", project=pid, name="ship")
assert {m["alias"]: m["state"] for m in show["modules"]} == {"checks": "current", "clean": "current", "review": "current"}, show["modules"]

# Module drift: a newer user module of the same name offers an update; the update needs approval.
user_mod = agent / "modules/validate"
user_mod.mkdir(parents=True)
for f in (pathlib.Path(names["validate"]["dir"])).rglob("*"):
    if f.is_file() and "__pycache__" not in f.parts:
        target = user_mod / f.relative_to(names["validate"]["dir"])
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(f.read_bytes())
(user_mod / "module.toml").write_text(
    (user_mod / "module.toml").read_text().replace("version = 1", "version = 2").replace('default = ""', 'default = "make test"'))
assert tomllib.loads((pf / "modules.lock").read_text())["modules"]["checks"]["params"] == ["validate_command"]
show = act("factory-show", project=pid, name="ship")
checks = next(m for m in show["modules"] if m["alias"] == "checks")
assert checks == {"alias": "checks", "module": "validate", "version": 1, "latest": 2, "state": "update"}, checks
assert show["state"] == "approved" and show["drift"] == "module update available", show
updated = act("factory-update-module", project=pid, name="ship", alias="checks")
assert updated["state"] == "changed", updated
assert tomllib.loads((pf / "modules.lock").read_text())["modules"]["checks"]["version"] == 2
assert tomllib.loads((pf / "machine.toml").read_text())["states"]["checks-run"]["route"]["true"] == "done"
# The update replaces the params the module added and wires its entry again.
manifest = tomllib.loads((pf / "machine.toml").read_text())
assert manifest["params"]["validate_command"]["default"] == "make test", manifest["params"]
assert manifest["states"]["ship"]["next"] == "checks-run", manifest["states"]["ship"]
assert (pf / "machine.toml").read_text().count("[params.validate_command]") == 1
assert next(m for m in act("factory-show", project=pid, name="ship")["modules"] if m["alias"] == "checks")["state"] == "current"
validate(pf)
act("factory-approve", project=pid, name="ship", digest=factory()["digest"])
print("Verified module vendoring, lock, drift, and update with re-approval.")

# A forged modules.lock cannot make an update delete or write outside the factory.
victim = r / "victim"; (victim / "keep").mkdir(parents=True); (victim / "keep/file").write_text("keep")
good_lock = (pf / "modules.lock").read_text()
before_toml = (pf / "machine.toml").read_text()


def forged(text, needle, alias):
    (pf / "modules.lock").write_text(text)
    refused("factory-update-module", needle, project=pid, name="ship", alias=alias)
    assert (victim / "keep/file").read_text() == "keep"
    assert (pf / "machine.toml").read_text() == before_toml
    (pf / "modules.lock").write_text(good_lock)


rel = os.path.relpath(victim / "keep", pf / "modules")
forged(good_lock + f'\n[modules."{rel}"]\nmodule = "validate"\nversion = 1\nstates = []\n', "not a module alias", rel)
forged(good_lock.replace('states = ["checks-run"]', 'states = ["checks-run", "build"]'), "not an checks- state", "checks")
(pf / "modules/evil").symlink_to(victim / "keep", target_is_directory=True)
forged(good_lock + '\n[modules.evil]\nmodule = "validate"\nversion = 1\nstates = ["evil-run"]\n', "symlink", "evil")
(pf / "modules/evil").unlink()
assert (pf / "machine.toml").read_text() == before_toml and factory()["state"] == "approved"
print("Verified that a forged modules.lock stays inside the factory.")

# Specialists: actor rules and path safety.
spec = dict(project=pid, id="theory", name="ML theory verifier", description="Checks proofs and bounds.",
            model="anthropic/claude-opus-4-6", thinking="high", prompt="Verify every bound you rely on.")
refused("specialist-write", "crew", actor="crew:" + pid, **spec)
refused("specialist-write", "turn that answers the user", actor=pid, **spec)
refused("specialist-write", "lowercase", **dict(spec, id="../evil"))
refused("specialist-write", "lowercase", **dict(spec, id="Evil"))
written = act("specialist-write", **spec)
assert written["model"] == "anthropic/claude-opus-4-6" and written["prompt"] == "Verify every bound you rely on.", written
text = (a / ".prime/agents/theory.md").read_text()
assert text.startswith("---\nname: ML theory verifier\n") and "thinking: high\n" in text, text
outside = r / "outside.md"; outside.write_text("---\nmodel: x\n---\nleak\n")
(a / ".prime/agents/link.md").symlink_to(outside)
refused("specialist-show", "symlink", project=pid, id="link")
refused("specialist-write", "symlink", **dict(spec, id="link"))
assert outside.read_text() == "---\nmodel: x\n---\nleak\n"
(a / ".prime/agents/link.md").unlink()
# A symlink planted at the old fixed temporary name is never followed.
(a / ".prime/agents/.theory.md.tmp").symlink_to(outside)
act("specialist-write", **spec)
assert outside.read_text() == "---\nmodel: x\n---\nleak\n" and not (a / ".prime/agents/theory.md").is_symlink()
(a / ".prime/agents/.theory.md.tmp").unlink()
listed = act("specialist-list", project=pid)["specialists"]
assert [s["id"] for s in listed] == ["theory"], listed
assert act("specialist-list", project=pid, actor=None)["specialists"]
crew_read = run("specialist-show", actor="crew:" + pid, project=pid, id="theory")
assert crew_read.returncode == 0, crew_read.stderr
refused("specialist-delete", "Use an action", actor=pid, project=pid, id="theory")

# A symlinked agents folder that leaves the project is refused.
b = r / "beta"; b.mkdir()
subprocess.check_call(["git", "-C", str(b), "init", "-q"])
(b / ".prime").mkdir(); (b / ".prime/agents").symlink_to(r / "elsewhere", target_is_directory=True)
(r / "elsewhere").mkdir()
bid = json.loads(subprocess.check_output([exe, "thread", "captain", "act", "init"], input=json.dumps({"objective": "b"}), text=True, env=env, cwd=b))["id"]
cp = subprocess.run([exe, "thread", "captain", "act", "specialist-write"], input=json.dumps(dict(spec, project=bid)), text=True, env=env, cwd=b, capture_output=True)
assert cp.returncode and "leaves the project folder" in cp.stderr, cp.stderr
assert not list((r / "elsewhere").iterdir())
act("stop", project=bid)

# Materialization into the factory: role, prompt block, lock digest; then approval.
used = act("factory-use-specialist", project=pid, name="ship", specialist="theory", states=["build"])
assert used["state"] == "changed", used
manifest = tomllib.loads((pf / "machine.toml").read_text())
assert manifest["roles"]["theory"] == {"model": "anthropic/claude-opus-4-6", "thinking": "high"}, manifest["roles"]
assert manifest["states"]["build"]["agent"] == {"role": "theory", "prompt": "prompts/build.md"}, manifest["states"]["build"]
prompt = (pf / "prompts/build.md").read_text()
assert prompt.startswith("<!-- pa specialist theory -->\nVerify every bound you rely on.\n") and prompt.endswith("Build the change."), prompt
assert tomllib.loads((pf / "modules.lock").read_text())["specialists"]["theory"]["digest"] == written["digest"]
validate(pf)
refused("factory-use-specialist", "not an agent state", project=pid, name="ship", specialist="theory", states=["checks-run"])
refused("factory-use-specialist", "crew", actor="crew:" + pid, project=pid, name="ship", specialist="theory", states=["build"])
act("factory-approve", project=pid, name="ship", digest=factory()["digest"])
assert factory()["state"] == "approved" and factory()["drift"] == ""
shown = act("specialist-show", project=pid, id="theory")
assert shown["used_by"] == ["ship"], shown

# Drift: the project file changes; the approved factory is flagged in status and Needs you.
act("specialist-write", **dict(spec, prompt="Verify every bound and every constant."))
row = factory()
assert row["state"] == "approved" and row["drift"] == "specialist changed - update", row
status_file = json.loads((agent / f"captains/status/{pid}.json").read_text())
assert any(i["id"] == f"{pid}/ship" and i["state_text"] == "specialist changed - update" for i in status_file["needs_you_items"]), status_file
assert next(f for f in status_file["factories"] if f["name"] == "ship")["drift"] == "specialist changed - update"
updated = act("factory-update-specialist", project=pid, name="ship", specialist="theory")
assert updated["state"] == "changed" and updated["drift"] == "", updated
prompt = (pf / "prompts/build.md").read_text()
assert prompt.count("<!-- pa specialist theory -->") == 1 and "every constant" in prompt and "rely on" not in prompt, prompt
validate(pf)
act("factory-approve", project=pid, name="ship", digest=factory()["digest"])
status_file = json.loads((agent / f"captains/status/{pid}.json").read_text())
assert not any(i["id"] == f"{pid}/ship" for i in status_file["needs_you_items"]), status_file
print("Verified specialist materialization, drift flag, and re-approval.")

# Crew delegation with a specialist: its model, thinking, and prompt reach the crew thread.
crew = act("delegate", project=pid, provider="claude", specialist="theory", task="check the bound in lemma 2")
assert wait_job(crew["job"])[0] == "done", wait_job(crew["job"])
member = next(m for m in status()["projects"][0]["crew"] if m["id"] == crew["id"])
assert member["model"] == "claude-opus-4-6" and member["thinking"] == "high" and member["specialist"] == "theory", member
calls = [json.loads(line) for line in call_log.read_text().splitlines()]
seen = json.dumps(calls[-1])
assert "claude-opus-4-6" in seen and "every constant" in seen, calls[-1]
reply = act("reply", project=pid, member=crew["id"], text="and lemma 3")
assert wait_job(reply["job"])[0] == "done"
assert "every constant" in json.dumps(json.loads(call_log.read_text().splitlines()[-1]))
refused("delegate", "No specialist", project=pid, provider="claude", specialist="nope", task="x")
print("Verified crew delegation with a specialist.")

# The user deletes a specialist; factories that vendored it show it as missing, in status too.
assert act("specialist-delete", project=pid, id="theory")["ok"]
status_file = json.loads((agent / f"captains/status/{pid}.json").read_text())
assert next(f for f in status_file["factories"] if f["name"] == "ship")["drift"] == "specialist missing - write it again", status_file
assert factory()["drift"] == "specialist missing - write it again"
assert next(s for s in act("factory-show", project=pid, name="ship")["specialists"] if s["id"] == "theory")["state"] == "missing"
refused("factory-update-specialist", "No specialist", project=pid, name="ship", specialist="theory")
# Writing it again is the next step: the factory then offers the update.
act("specialist-write", **spec)
assert factory()["drift"] == "specialist changed - update"
act("stop", project=pid)
with sqlite3.connect(db_path) as c:
    c.execute("delete from watches")  # the Captain watcher exits with no watches
print("ok")
