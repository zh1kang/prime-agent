"""Factory models: the setup choice is stored on the project, reaches the design turn, sets the
model that inheriting steps resolve to, and a default model change needs approval again.
Usage: python3 captain_factory_models.py PATH_TO_PA TEMP_DIR"""
import atexit, json, os, pathlib, sqlite3, subprocess, sys, time

P = pathlib.Path
binary = str(P(sys.argv[1]).resolve())
r = P(sys.argv[2]).resolve()
a = r / "alpha"; a.mkdir()
agent = r / "agent"; agent.mkdir()
(agent / "settings.json").write_text(json.dumps(dict(factory=dict(enabled=True), onboardingShown=True, agentTraces=dict(enabled=False))))
# Prime turns start a daemon on this socket; stop it when the test ends.
SOCK = f"/tmp/pa-cfm-{os.getpid()}.sock"
atexit.register(subprocess.run, ["pkill", "-f", SOCK], check=False)
env = dict(os.environ, PRIME_AGENT_CODING_AGENT_DIR=str(agent), PA_CAPTAIN_HOME=str(agent), PA_CAPTAIN_NATIVE_HOME=str(agent),
           PRIME_AGENT_DAEMON_SOCKET=SOCK, PA_CAPTAIN_NATIVE_SOCKET=SOCK, DO_NOT_TRACK="1",
           PRIME_AGENT_FAUX_SCRIPT=json.dumps({"responses": ["Captain received the brief."], "repeatLastResponse": True}))
for key in ("PRIME_AGENT_KERNEL_PYTHON", "PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION", "RLM_SESSION_ID"):
    env.pop(key, None)
MODEL = "anthropic/claude-opus-5-5"


def run(*args, body=None, allow=False):
    cp = subprocess.run([binary, "thread", *args], cwd=a, env=env, input=body, text=True, capture_output=True, timeout=120)
    if not allow and cp.returncode:
        raise RuntimeError(str(args) + " " + cp.stderr[-1500:])
    return cp


def act(op, **args): return json.loads(run("captain", "act", op, body=json.dumps(args)).stdout)
def db(): return sqlite3.connect(agent / "captains/projects.sqlite3")


def wait(f, what, limit=120):
    for _ in range(int(limit / 0.25)):
        res = f()
        if res:
            return res
        time.sleep(0.25)
    raise RuntimeError("timeout " + what + " " + str(db().execute("select purpose,status,result from jobs").fetchall()))


subprocess.check_call(["git", "-C", str(a), "init", "-q"])
subprocess.check_call(["git", "-C", str(a), "-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "--allow-empty", "-qm", "base"])

# A choice that names no model is refused before any project exists.
bad = run("captain", "act", "init", body=json.dumps({"brief": "Ship Alpha", "design": True, "factory_models": {"mode": "model"}}), allow=True)
assert bad.returncode and "Choose the model for every factory step" in bad.stderr, bad.stderr
assert not (agent / "captains/projects.sqlite3").exists() or not db().execute("select 1 from projects").fetchone()

pid = act("init", brief="Ship Alpha", design=True, factory_models={"mode": "model", "model": MODEL, "thinking": "high"})["id"]
stored = json.loads(db().execute("select factory_models from projects where id=?", (pid,)).fetchone()[0])
assert stored == {"mode": "model", "model": MODEL, "thinking": "high"}, stored
print("Verified the factory model choice is stored on the project.", flush=True)

# The design turn reads the choice as hidden context.
wait(lambda: db().execute("select 1 from jobs where purpose='design' and status not in ('queued','running')").fetchone(), "design turn")
lead = db().execute("select lead from projects").fetchone()[0]
session = json.loads((agent / "threads" / (lead + ".json")).read_text())["native"]
hidden = [row for row in map(json.loads, P(session).read_text().splitlines())
          if "Design request from the user" in json.dumps(row)]
assert hidden, "the design turn has no hidden context"
expected = f'[roles] default = {{ model = \\"{MODEL}\\", thinking = \\"high\\" }}'
assert expected in json.dumps(hidden[-1]), json.dumps(hidden[-1])[-1500:]
assert 'agent = { role = \\"default\\" }' in json.dumps(hidden[-1])
print("Verified the design turn gets one default role with the chosen model.", flush=True)

# A step with no model or role resolves to the project's choice, and a default change is an
# edit that needs approval again.
pf = a / ".prime/machines/proj-plan"; (pf / "prompts").mkdir(parents=True)
(pf / "prompts/plan.md").write_text("Plan the work.\n")
(pf / "machine.toml").write_text('format = 1\nname = "proj-plan"\nversion = 1\ndescription = "Plan."\n\n'
                                 '[states.plan]\nentry = true\nagent = { prompt = "prompts/plan.md" }\noutputs = { plan = "text" }\n')
show = act("factory-show", project=pid, name="proj-plan")
assert show["inherit_model"] == {"model": MODEL, "thinking": "high"}, show.get("inherit_model")
assert "default_role" not in show
act("factory-approve", project=pid, name="proj-plan", digest=show["digest"])
changed = act("factory-set-default-model", project=pid, name="proj-plan", model="anthropic/claude-sonnet-4-6", thinking="low")
assert changed["state"] == "changed" and "needs approval" in changed["notice"], changed
assert changed["default_role"] == {"model": "anthropic/claude-sonnet-4-6", "thinking": "low"}, changed
assert 'default = { model = "anthropic/claude-sonnet-4-6", thinking = "low" }' in (pf / "machine.toml").read_text()
refused = run("captain", "act", "factory-set-default-model", body=json.dumps(
    dict(project=pid, name="proj-plan", model=MODEL)), allow=True)
assert refused.returncode == 0  # the user may change it again
crew = subprocess.run([binary, "thread", "captain", "act", "factory-set-default-model"], cwd=a, env=dict(env, PA_CAPTAIN_ACTOR=pid),
                      input=json.dumps(dict(project=pid, name="proj-plan", model=MODEL)), text=True, capture_output=True)
assert crew.returncode and "Use an action in this Captain" in crew.stderr, crew.stderr
print("Verified the resolved default model and that a default model change needs approval again.", flush=True)

act("stop", project=pid)
with db() as c:
    c.execute("delete from watches")
print("ok")
