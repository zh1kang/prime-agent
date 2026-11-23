"""pa's machine editor on a project factory: draft, graph, read, write, failed and good saves,
role/state model+thinking edits, factory-set-model approval invalidation, and crew model persistence.
Usage: captain_factory_edit.py PA TEMP"""
import atexit, json, os, pathlib, sqlite3, subprocess, sys, time
exe = sys.argv[1]
r = pathlib.Path(sys.argv[2]).resolve()
a = r / "alpha"; a.mkdir(); agent = r / "agent"; agent.mkdir()
mock = r / "bin"; mock.mkdir()
model_log = r / "claude-models.log"
(mock / "claude").write_text(f"""#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
model = ""
for i, arg in enumerate(args):
    if arg == "--model" and i + 1 < len(args):
        model = args[i + 1]
with open({str(model_log)!r}, "a") as fh:
    fh.write(model + "\\n")
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
pf = a / ".prime/machines/proj-build"; (pf / "nodes").mkdir(parents=True)
toml = ('format = 1\nname = "proj-build"\nversion = 1\ndescription = "Write a marker."\n'
        '[states.build]\nentry = true\nkind = "python"\nrun = "nodes/build.py:build"\noutputs = { built = "text" }\n')
(pf / "machine.toml").write_text(toml)
(pf / "nodes/build.py").write_text("def build(inputs):\n    return {'built': 'yes'}\n")

# Second project factory with [roles], role-backed agent state, inline agent state, and python state.
pr = a / ".prime/machines/proj-roles"
(pr / "prompts").mkdir(parents=True)
(pr / "nodes").mkdir(parents=True)
roles_toml = (
    '# keep this header comment\n'
    'format = 1\nname = "proj-roles"\nversion = 1\ndescription = "Role and state model edits."\n\n'
    '[roles.coder]\nmodel = "anthropic/claude-sonnet-4-6"\nthinking = "medium"\n\n'
    '[roles.reviewer]\nmodel = "openai-codex/gpt-5.3-codex"\n\n'
    '[states.implement]\nentry = true\nagent = { role = "coder", prompt = "prompts/impl.md" }\n'
    'outputs = { diff = "text" }\nnext = "review"\n\n'
    '[states.review]\nagent = { model = "anthropic/claude-sonnet-4-6", prompt = "prompts/review.md" } # a comment with }\n'
    'inputs = { diff = "implement.diff" }\noutputs = { ok = "text" }\nnext = "check"\n\n'
    '[states.check]\nkind = "python"\nrun = "nodes/check.py:check"\n'
    'inputs = { ok = "review.ok" }\noutputs = { done = "text" }\n'
)
(pr / "machine.toml").write_text(roles_toml)
(pr / "prompts/impl.md").write_text("Implement the change.")
(pr / "prompts/review.md").write_text("Review the diff.")
(pr / "nodes/check.py").write_text("def check(inputs):\n    return {'done': inputs['ok']}\n")

def act(op, **d):
    return json.loads(subprocess.check_output([exe, "thread", "captain", "act", op], input=json.dumps(d), text=True, env=env, cwd=a))
def fsm(op, **d):
    cp = subprocess.run([exe, "thread", "fsm-edit", op], input=json.dumps(d), text=True, env=env, cwd=a, capture_output=True)
    return json.loads(cp.stdout) if cp.returncode == 0 else ("ERR", cp.stderr.strip())
pid = act("init", objective="x")["id"]
def factory(name="proj-build"):
    st = json.loads(subprocess.check_output([exe, "thread", "captain", "status"], text=True, env=env, cwd=a))
    return next(f for f in st["projects"][0]["factories"] if f["name"] == name)
f = factory(); act("factory-approve", project=pid, name="proj-build", digest=f["digest"])
assert factory()["state"] == "approved", factory()
d = fsm("draft", name="proj-build", project=str(a))
assert d["project"] == str(a) and d["graph"] and d["graph"]["states"], d
print("graph states:", [s["id"] for s in d["graph"]["states"]])
assert fsm("read", draft=d["draft"])["text"] == toml
bad = fsm("write", draft=d["draft"], text="format = 1\nname = \"proj-build\"\n[states")
assert bad["graph"] is None and bad["graph_error"], bad
err = fsm("save", draft=d["draft"])
assert err[0] == "ERR" and "Validation failed" in err[1] and (pf / "machine.toml").read_text() == toml, err
w = fsm("write", draft=d["draft"], text=toml.replace("Write a marker.", "Write a marker twice."))
assert w["graph"], w
assert fsm("save", draft=d["draft"]) == {"saved": "proj-build"}
assert "twice" in (pf / "machine.toml").read_text()
assert factory()["state"] == "changed", factory()
print("project factory edit: saved, digest changed, state", factory()["state"])
assert fsm("draft", name="../x", project=str(a))[0] == "ERR"
assert fsm("draft", name="nope", project=str(a))[0] == "ERR"

# machine.toml role + state model/thinking edits round-trip pa factory validate.
rd = fsm("draft", name="proj-roles", project=str(a))
assert rd["roles"]["coder"]["model"] == "anthropic/claude-sonnet-4-6", rd
by_id = {s["id"]: s for s in rd["states"]}
assert by_id["implement"]["editable"] is True and by_id["implement"]["role"] == "coder", by_id
assert by_id["review"]["editable"] is True and by_id["review"]["raw_model"] == "anthropic/claude-sonnet-4-6", by_id
assert by_id["check"]["editable"] is False and by_id["check"]["kind"] == "python", by_id

# Edit role-backed state's role model + thinking, and inline state's model + thinking.
u1 = fsm("set-model", draft=rd["draft"], state="implement", model="openai-codex/gpt-5.4", thinking="high", edit_role=True)
assert u1["roles"]["coder"] == {"model": "openai-codex/gpt-5.4", "thinking": "high"}, u1
u2 = fsm("set-model", draft=rd["draft"], state="review", model="anthropic/claude-opus-4-6", thinking="low")
u2_rev = next(s for s in u2["states"] if s["id"] == "review")
assert u2_rev["raw_model"] == "anthropic/claude-opus-4-6" and u2_rev["thinking"] == "low", u2_rev
u3 = fsm("set-role-model", draft=rd["draft"], role="reviewer", model="anthropic/claude-opus-4-6", thinking="max")
assert u3["roles"]["reviewer"] == {"model": "anthropic/claude-opus-4-6", "thinking": "max"}, u3
assert fsm("save", draft=rd["draft"]) == {"saved": "proj-roles"}
saved_toml = (pr / "machine.toml").read_text()
assert "# keep this header comment" in saved_toml, "comments preserved"
subprocess.check_call([exe, "factory", "validate", str(pr)], env=env, stdout=subprocess.DEVNULL)

# Approve proj-roles, then change a node model via factory-set-model -> approval invalidated.
fr = factory("proj-roles")
act("factory-approve", project=pid, name="proj-roles", digest=fr["digest"])
assert factory("proj-roles")["state"] == "approved"
# Captain actor cannot run factory-set-model.
cap_cp = subprocess.run([exe, "thread", "captain", "act", "factory-set-model"],
    input=json.dumps({"project": pid, "name": "proj-roles", "state": "implement", "model": "anthropic/claude-sonnet-4-6"}),
    text=True, env=dict(env, PA_CAPTAIN_ACTOR=pid), cwd=a, capture_output=True)
assert cap_cp.returncode != 0, cap_cp.stdout
updated = act("factory-set-model", project=pid, name="proj-roles", state="implement",
              model="anthropic/claude-opus-4-6", thinking="high", edit_role=True)
assert updated["state"] == "changed" and "notice" in updated, updated
assert updated["state_details"]["implement"]["model"] == "anthropic/claude-opus-4-6", updated
assert factory("proj-roles")["state"] == "changed"
subprocess.check_call([exe, "factory", "validate", str(pr)], env=env, stdout=subprocess.DEVNULL)
def refused(op, needle, **d):
    cp = subprocess.run([exe, "thread", "captain", "act", op], input=json.dumps(d), text=True, env=env, cwd=a, capture_output=True)
    assert cp.returncode and needle in cp.stderr, (op, needle, cp.stdout, cp.stderr)
refused("factory-set-model", "Unknown prime model", project=pid, name="proj-roles", state="review", model="nope/nothing")
refused("factory-set-model", "thinking level", project=pid, name="proj-roles", state="review",
        model="anthropic/claude-opus-4-6", thinking="loud")
print("Verified machine.toml role/state model edits and factory-set-model approval invalidation.")

# The editor and delete never follow a symlink out of the project.
outside_file = r / "outside.py"; outside_file.write_text("SECRET = 1\n")
(pf / "nodes/leak.py").symlink_to(outside_file)
leak = fsm("draft", name="proj-build", project=str(a))
assert leak[0] == "ERR" and "outside" in leak[1], leak
(pf / "nodes/leak.py").unlink()
c = r / "gamma"; c.mkdir()
subprocess.check_call(["git", "-C", str(c), "init", "-q"])
elsewhere = r / "elsewhere"; (elsewhere / "machines/x").mkdir(parents=True)
(elsewhere / "machines/x/machine.toml").write_text(toml.replace("proj-build", "x"))
(c / ".prime").symlink_to(elsewhere, target_is_directory=True)
linked = subprocess.run([exe, "thread", "fsm-edit", "draft"], input=json.dumps({"name": "x", "project": str(c)}),
                        text=True, env=env, cwd=c, capture_output=True)
assert linked.returncode and "symlink" in linked.stderr, linked.stderr
cid = json.loads(subprocess.check_output([exe, "thread", "captain", "act", "init"], input=json.dumps({"objective": "c"}), text=True, env=env, cwd=c))["id"]
deleted = subprocess.run([exe, "thread", "captain", "act", "factory-delete"], input=json.dumps({"project": cid, "name": "x"}),
                         text=True, env=env, cwd=c, capture_output=True)
assert deleted.returncode and "symlink" in deleted.stderr, deleted.stderr
(c / ".prime").unlink(); (c / ".prime").mkdir()
(c / ".prime/machines").symlink_to(elsewhere / "machines", target_is_directory=True)
deleted = subprocess.run([exe, "thread", "captain", "act", "factory-delete"], input=json.dumps({"project": cid, "name": "x"}),
                         text=True, env=env, cwd=c, capture_output=True)
assert deleted.returncode and "symlink" in deleted.stderr, deleted.stderr
assert (elsewhere / "machines/x/machine.toml").is_file()
act("stop", project=cid)
print("Verified that the editor and delete refuse symlinks that leave the project.")

# Crew model + thinking persist across reply turns and update via crew-model.
db_path = agent / "captains/projects.sqlite3"
def wait_job(jid):
    for _ in range(120):
        with sqlite3.connect(db_path) as conn:
            row = conn.execute("select status,result from jobs where id=?", (jid,)).fetchone()
        if row and row[0] not in ("queued", "running"):
            return row
        time.sleep(0.2)
    raise RuntimeError(f"timeout waiting for job {jid}")

wait_job(sqlite3.connect(db_path).execute("select id from jobs where purpose='brief'").fetchone()[0])
crew = act("delegate", project=pid, provider="claude", model="claude-sonnet-4-6", thinking="high", task="first turn")
assert wait_job(crew["job"])[0] == "done"
# Crew row stores model + thinking and exposes them in status snapshot.
st = json.loads(subprocess.check_output([exe, "thread", "captain", "status"], text=True, env=env, cwd=a))
cm = next(m for m in st["projects"][0]["crew"] if m["id"] == crew["id"])
assert cm["model"] == "claude-sonnet-4-6" and cm["thinking"] == "high", cm
# Reply inherits model + thinking without re-specifying them.
rep1 = act("reply", project=pid, member=crew["id"], text="second turn")
assert wait_job(rep1["job"])[0] == "done"
# crew-model changes model + thinking and subsequent reply uses the new values.
cm_upd = act("crew-model", project=pid, member=crew["id"], model="claude-opus-4-6", thinking="max")
assert cm_upd["model"] == "claude-opus-4-6" and cm_upd["thinking"] == "max", cm_upd
rep2 = act("reply", project=pid, member=crew["id"], text="third turn")
assert wait_job(rep2["job"])[0] == "done"
logged_models = model_log.read_text().splitlines()
assert logged_models == ["claude-sonnet-4-6", "claude-sonnet-4-6", "claude-opus-4-6"], logged_models
print("Verified crew model + thinking persist across replies and update via crew-model.")

act("stop", project=pid)
print("ok")
