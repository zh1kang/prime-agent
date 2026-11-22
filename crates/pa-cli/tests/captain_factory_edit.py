"""pa's machine editor on a project factory: draft, graph, read, write, failed and good saves.
A saved edit changes the digest, so an approved factory needs approval again.
Usage: captain_factory_edit.py PA TEMP"""
import json, os, pathlib, subprocess, sys
exe = sys.argv[1]
r = pathlib.Path(sys.argv[2]).resolve()
a = r / "alpha"; a.mkdir(); agent = r / "agent"; agent.mkdir()
env = dict(
    os.environ,
    PRIME_AGENT_CODING_AGENT_DIR=str(agent),
    PA_CAPTAIN_HOME=str(agent),
    PA_CAPTAIN_NATIVE_HOME=str(agent),
    PRIME_AGENT_DAEMON_SOCKET=str(r / "d.sock"),
    PA_CAPTAIN_NATIVE_SOCKET=str(r / "d.sock"),
    PRIME_AGENT_FAUX_SCRIPT=json.dumps({"responses": ["Captain received the brief."]}),
    DO_NOT_TRACK="1",
)
for key in ("PRIME_AGENT_KERNEL_PYTHON", "PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION"):
    env.pop(key, None)
subprocess.check_call(["git", "-C", str(a), "init", "-q"])
pf = a / ".prime/machines/proj-build"; (pf / "nodes").mkdir(parents=True)
toml = ('format = 1\nname = "proj-build"\nversion = 1\ndescription = "Write a marker."\n'
        '[states.build]\nentry = true\nkind = "python"\nrun = "nodes/build.py:build"\noutputs = { built = "text" }\n')
(pf / "machine.toml").write_text(toml)
(pf / "nodes/build.py").write_text("def build(inputs):\n    return {'built': 'yes'}\n")
def act(op, **d):
    return json.loads(subprocess.check_output([exe, "thread", "captain", "act", op], input=json.dumps(d), text=True, env=env, cwd=a))
def fsm(op, **d):
    cp = subprocess.run([exe, "thread", "fsm-edit", op], input=json.dumps(d), text=True, env=env, cwd=a, capture_output=True)
    return json.loads(cp.stdout) if cp.returncode == 0 else ("ERR", cp.stderr.strip())
pid = act("init", objective="x")["id"]
def factory():
    st = json.loads(subprocess.check_output([exe, "thread", "captain", "status"], text=True, env=env, cwd=a))
    return next(f for f in st["projects"][0]["factories"] if f["name"] == "proj-build")
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
act("stop", project=pid)
print("ok")
