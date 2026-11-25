"""Who runs a Captain command: the actor a session has from the project records, the CLI
shortcuts only the user may run, and the factory host's model.
Usage: python3 captain_actors.py PATH_TO_PA [TEMP_DIR]"""
import json, os, pathlib, sqlite3, subprocess, sys, tempfile, uuid

P = pathlib.Path
SRC = P(__file__).resolve().parent.parent / "src"
binary = str(P(sys.argv[1]).resolve())
temp = P(tempfile.mkdtemp(prefix="pa-captain-actors-", dir=sys.argv[2] if len(sys.argv) > 2 else "/tmp")).resolve()
hub = temp / "hub"; native = temp / "native"; root = temp / "proj"
(native / "sessions").mkdir(parents=True); root.mkdir()
for key in ("PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION", "RLM_SESSION_ID", "PRIME_AGENT_SESSION_DIR", "PRIME_AGENT_CODING_AGENT_DIR"):
    os.environ.pop(key, None)
os.environ.update(PA_CAPTAIN_HOME=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native), DO_NOT_TRACK="1")

sys.argv = ["captain", binary]
captain = {"__name__": "captain"}
exec(compile("\n".join((SRC / f).read_text() for f in ("captain_backend.py", "machine_catalog.py", "captain_jobs.py")),
             "captain", "exec"), captain)


def session(name, **header):
    sid = str(uuid.uuid4())
    path = native / "sessions" / f"{name}_{sid}.jsonl"
    path.write_text(json.dumps(dict(type="session", version=3, id=sid, cwd=str(root), **header)) + "\n")
    return sid, str(path)


def actor(sid, env_actor=None):
    os.environ["RLM_SESSION_ID"] = sid
    if env_actor:
        os.environ["PA_CAPTAIN_ACTOR"] = env_actor
    else:
        os.environ.pop("PA_CAPTAIN_ACTOR", None)
    try:
        return captain["effective_actor"]()
    finally:
        os.environ.pop("RLM_SESSION_ID", None)
        os.environ.pop("PA_CAPTAIN_ACTOR", None)


lead_sid, lead_path = session("lead")
user_sid, user_path = session("user")
host_sid, host_path = session("host")
fork_sid, _ = session("fork", parentSession=lead_path, rlmDepth=0)
sub_sid, _ = session("sub", parentSession=lead_path, rlmDepth=1)
lead_tid = str(uuid.uuid4())
(hub / "threads").mkdir(parents=True)
(hub / "threads" / (lead_tid + ".json")).write_text(json.dumps(dict(provider="prime", native=lead_path)))

with captain["database"]():
    pass
pid = "alpha"
with sqlite3.connect(hub / "captains/projects.sqlite3") as db:
    db.execute("INSERT INTO projects(id,root,name,objective,lead) VALUES(?,?,?,?,?)", (pid, str(root), "Alpha", "Ship", lead_tid))
    row = "INSERT INTO crew(id,project,kind,ref,title,cwd,provider,state,updated) VALUES(?,?,'factory',?,'run',?,'prime','running',0)"
    # A run the user started from their own chat, which the Captain watches, and a run the
    # Captain started headless, whose ref is its host session.
    db.execute(row, (str(uuid.uuid4()), pid, user_path + "#" + "0" * 32, str(root)))
    db.execute(row, (str(uuid.uuid4()), pid, host_path, str(root)))

assert actor(lead_sid) == pid
assert actor(sub_sid) == pid, "a subagent of the lead has the lead's actor"
assert actor(fork_sid) == "", "a user /fork of the Captain chat is a normal chat"
assert actor(user_sid) == "", "a chat that runs a watched factory stays the user's"
assert actor(host_sid) == "crew:" + pid, "a Captain-started factory host is crew"
assert actor(host_sid, "crew:" + pid) == "crew:" + pid
assert actor(user_sid, "crew:" + pid) == "crew:" + pid
for sid, env_actor in ((host_sid, "crew:beta"), (lead_sid, "beta"), (host_sid, "beta")):
    try:
        actor(sid, env_actor)
    except ValueError as error:
        assert "different Captain" in str(error), error
    else:
        raise AssertionError(f"{env_actor} in the session of {captain['session_actor']()} was not refused")
print("Verified session actors: forks and watched factories stay the user's, and a mismatched actor is refused.", flush=True)

project = dict(id=pid, state="active", lead=lead_tid)
for env_actor in ("crew:" + pid, pid):
    os.environ["PA_CAPTAIN_ACTOR"] = env_actor
    for op in ("stop", "resume", "delete"):
        try:
            captain["project_state_action"](op, project)
        except ValueError as error:
            assert "Only you" in str(error), error
        else:
            raise AssertionError(f"{env_actor} ran {op}")
os.environ.pop("PA_CAPTAIN_ACTOR")
env = dict(os.environ)
for env_actor, rlm in (("crew:" + pid, None), (None, host_sid), (pid, None)):
    run_env = dict(env)
    if env_actor:
        run_env["PA_CAPTAIN_ACTOR"] = env_actor
    if rlm:
        run_env["RLM_SESSION_ID"] = rlm
    for op in ("stop", "resume", "delete"):
        cp = subprocess.run([binary, "thread", "captain", op, pid], cwd=root, env=run_env, text=True, capture_output=True, timeout=60)
        assert cp.returncode and "Only you can" in cp.stderr, (env_actor, rlm, op, cp.stdout, cp.stderr)
with sqlite3.connect(hub / "captains/projects.sqlite3") as db:
    assert db.execute("SELECT state FROM projects").fetchone() == ("active",)
print("Verified a crew, factory host, or Captain session cannot stop, resume, or delete a Captain.", flush=True)

host = captain["machine_host_model"]
assert host({"thinking": "high"}, ("prime/m1", "low")) == ("prime/m1", "high")
assert host({"model": "prime/m2"}, ("prime/m1", "low")) == ("prime/m2", "")
assert host({"model": "prime/m2", "thinking": "max"}, ("prime/m1", "low")) == ("prime/m2", "max")
assert host({}, ("prime/m1", "low")) == ("prime/m1", "low")
print("Verified a run's thinking level reaches the factory host without a model override.", flush=True)
