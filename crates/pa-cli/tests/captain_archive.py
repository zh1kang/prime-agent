"""Captain archive and unarchive: the store migration that frees a root, the archive paths
from paused and from running work, a new Captain beside an archived one, and unarchive.
Usage: python3 captain_archive.py PATH_TO_PA [TEMP_DIR]"""
import atexit, json, os, pathlib, sqlite3, subprocess, sys, tempfile, time

P = pathlib.Path
SRC = P(__file__).resolve().parent.parent / "src"
binary = str(P(sys.argv[1]).resolve())
temp = P(tempfile.mkdtemp(prefix="pa-captain-archive-", dir=sys.argv[2] if len(sys.argv) > 2 else "/tmp")).resolve()
a = temp / "alpha"; a.mkdir()
hub = temp / "hub"; native = temp / "native"; native.mkdir()
mock = temp / "bin"; mock.mkdir()
print(temp, flush=True)

# A store from schema version 2: root is UNIQUE and later columns were added with ALTER TABLE.
legacy = temp / "legacy"
(legacy / "captains").mkdir(parents=True)
with sqlite3.connect(legacy / "captains" / "projects.sqlite3") as c:
    c.execute("PRAGMA foreign_keys=ON")
    c.executescript("""
        CREATE TABLE projects (
            id TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
            objective TEXT NOT NULL, rules TEXT NOT NULL DEFAULT '', plan TEXT NOT NULL DEFAULT '',
            crew_rules TEXT NOT NULL DEFAULT '', lead TEXT NOT NULL UNIQUE);
        CREATE TABLE crew (
            id TEXT PRIMARY KEY, project TEXT NOT NULL REFERENCES projects(id),
            kind TEXT NOT NULL, ref TEXT NOT NULL, title TEXT NOT NULL, cwd TEXT NOT NULL,
            provider TEXT NOT NULL, state TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
            updated REAL NOT NULL, UNIQUE(kind,ref));
        CREATE TABLE edits (
            project TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE, field TEXT NOT NULL,
            old_len INTEGER NOT NULL, new_len INTEGER NOT NULL, actor TEXT NOT NULL, stamp REAL NOT NULL);
        ALTER TABLE projects ADD COLUMN parallel INTEGER NOT NULL DEFAULT 2;
        ALTER TABLE projects ADD COLUMN provider TEXT NOT NULL DEFAULT 'prime';
        ALTER TABLE projects ADD COLUMN state TEXT NOT NULL DEFAULT 'active';
        ALTER TABLE projects ADD COLUMN wake_policy TEXT NOT NULL DEFAULT '{}';
        ALTER TABLE projects ADD COLUMN budget_usd REAL;
        PRAGMA user_version=2;
    """)
    c.execute("INSERT INTO projects(id,root,name,objective,lead,parallel,provider,state,wake_policy,budget_usd) "
              "VALUES('gone','/r/gone','Gone','x','lead-gone',2,'prime','active','{}',NULL)")
    c.execute("INSERT INTO projects(id,root,name,objective,lead,parallel,provider,state,wake_policy,budget_usd) "
              "VALUES('old','/r/old','Old','Ship it','lead-old',5,'claude','paused','{\"done\":\"context\"}',3.5)")
    c.execute("DELETE FROM projects WHERE id='gone'")
    c.execute("INSERT INTO crew(id,project,kind,ref,title,cwd,provider,state,updated) "
              "VALUES('m1','old','thread','t1','Worker','/r/old','claude','ready',1)")
    c.execute("INSERT INTO edits(project,field,old_len,new_len,actor,stamp) VALUES('old','plan',0,4,'user',1)")
    before = c.execute("SELECT rowid,* FROM projects").fetchall()


def legacy_store():
    db = sqlite3.connect(legacy / "captains" / "projects.sqlite3")
    db.execute("PRAGMA foreign_keys=ON")
    return db


def open_legacy():
    """Open the legacy store with the backend, as the first `pa thread captain` call does."""
    run = subprocess.run(
        [sys.executable, "-c",
         "import sys; src = sys.argv[1]; sys.argv = ['captain', sys.argv[2]]; g = {'__name__': 'captain'}; "
         "code = '\\n'.join(open(src + '/' + f).read() for f in ('captain_backend.py', 'machine_catalog.py', 'captain_jobs.py')); "
         "exec(compile(code, 'captain', 'exec'), g)\n"
         "with g['database']() as db: pass",
         str(SRC), binary],
        env=dict(os.environ, PA_CAPTAIN_HOME=str(legacy), PA_CAPTAIN_NATIVE_HOME=str(native), DO_NOT_TRACK="1"),
        capture_output=True, text=True, timeout=60,
    )
    assert run.returncode == 0, run.stderr[-2000:]


open_legacy()
with legacy_store() as c:
    sql = c.execute("SELECT sql FROM sqlite_master WHERE type='table' AND name='projects'").fetchone()[0]
    assert "UNIQUE" not in sql.split("lead")[0] and "lead TEXT NOT NULL UNIQUE" in sql, sql
    index = c.execute("SELECT sql FROM sqlite_master WHERE type='index' AND name='projects_active_root'").fetchone()
    assert index and "WHERE state != 'archived'" in index[0], index
    assert c.execute("PRAGMA user_version").fetchone()[0] == 3
    cols = [r[1] for r in c.execute("PRAGMA table_info(projects)")]
    after = c.execute(f"SELECT rowid,{','.join(cols[:len(before[0]) - 1])} FROM projects").fetchall()
    assert after == before, (before, after)
    assert {"factory_models", "budget_tokens", "context_state"} <= set(cols), cols
    assert c.execute("SELECT count(*) FROM crew").fetchone()[0] == 1
    assert c.execute("SELECT count(*) FROM edits").fetchone()[0] == 1, "the rebuild cascaded a delete"
    assert c.execute("PRAGMA foreign_key_check").fetchall() == []
    try:
        c.execute("INSERT INTO crew(id,project,kind,ref,title,cwd,provider,state,updated) "
                  "VALUES('m2','nope','thread','t2','W','/r','claude','ready',1)")
        raise AssertionError("crew accepted a missing project")
    except sqlite3.IntegrityError:
        pass
    try:
        c.execute("INSERT INTO projects(id,root,name,objective,lead) VALUES('dup','/r/old','Dup','x','lead-dup')")
        raise AssertionError("a second Captain took a root that is not archived")
    except sqlite3.IntegrityError:
        pass
    c.execute("UPDATE projects SET state='archived' WHERE id='old'")
    c.execute("INSERT INTO projects(id,root,name,objective,lead) VALUES('new','/r/old','New','x','lead-new')")
    c.rollback()
open_legacy()
with legacy_store() as c:
    assert c.execute("SELECT rowid,* FROM projects").fetchall()[0][:len(before[0])] == before[0]
print("Verified the migration drops UNIQUE on root, keeps rows, columns, and references, and is safe to repeat.", flush=True)

(mock / "claude").write_text("""#!/usr/bin/env python3
import json, sys, time
body = sys.stdin.read()
if "slow" in body and "is your assignment" in body:
    time.sleep(120)
print(json.dumps({"type": "result", "session_id": "fa16e11d-0a87-47ae-a8d1-f6e3a12b806f", "result": "crew done", "is_error": False}))
""")
(mock / "claude").chmod(0o755)
(native / "settings.json").write_text(json.dumps(dict(onboardingShown=True, agentTraces=dict(enabled=False))))
SOCK = f"/tmp/pa-ca-{os.getpid()}.sock"
atexit.register(subprocess.run, ["pkill", "-f", SOCK], check=False)
env = dict(os.environ, PA_CAPTAIN_HOME=str(hub), PRIME_AGENT_CODING_AGENT_DIR=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native),
           PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"), PA_CAPTAIN_NATIVE_SOCKET=SOCK,
           DO_NOT_TRACK="1", PATH=str(mock) + ":" + os.environ["PATH"],
           PRIME_AGENT_FAUX_SCRIPT=json.dumps({"responses": ["Captain received the report."] * 12}))
for key in ("PRIME_AGENT_KERNEL_PYTHON", "PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION"):
    env.pop(key, None)


def run(*args, body=None, allow=False):
    cp = subprocess.run([binary, "thread", *args], cwd=a, env=env, input=body, text=True, capture_output=True, timeout=120)
    if not allow and cp.returncode:
        raise RuntimeError(str(args) + " " + cp.stderr[-1500:])
    return cp


def act(op, **args): return json.loads(run("captain", "act", op, body=json.dumps(args)).stdout)
def fails(op, **args):
    cp = run("captain", "act", op, body=json.dumps(args), allow=True)
    assert cp.returncode, (op, cp.stdout)
    return cp.stderr
def stat(): return json.loads(run("captain", "status").stdout)
def db(): return sqlite3.connect(hub / "captains/projects.sqlite3")
def state(pid): return db().execute("select state from projects where id=?", (pid,)).fetchone()[0]
def roots(): return json.loads((hub / "captains" / "roots.json").read_text())
def index(): return json.loads((hub / "captains" / "status" / "index.json").read_text())


def wait(f, limit=90, what=""):
    for _ in range(int(limit / 0.25)):
        res = f()
        if res:
            return res
        time.sleep(0.25)
    raise RuntimeError(f"timeout {what} " + str(db().execute("select purpose,status,result from jobs").fetchall()))


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def blocked(pid):
    assert "archived" in fails("lead", project=pid, text="hello")
    assert "archived" in fails("delegate", project=pid, provider="claude", task="x")


root = str(a.resolve())
try:
    subprocess.check_call(["git", "-C", str(a), "init", "-q"])
    subprocess.check_call(["git", "-C", str(a), "-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "--allow-empty", "-qm", "base"])
    first = act("init", objective="Ship Alpha")["id"]
    wait(lambda: db().execute("select 1 from jobs where purpose='brief' and status='done'").fetchone(), what="brief")
    assert roots() == [root] and index() == {root: first}

    # Only the user archives a Captain; the Captain may still archive its crew threads.
    env["PA_CAPTAIN_ACTOR"] = first
    try:
        assert "Only you" in fails("archive", project=first)
    finally:
        env.pop("PA_CAPTAIN_ACTOR")

    # Paused -> archived at once, without the member argument.
    assert json.loads(run("captain", "stop").stdout)["state"] == "paused"
    assert act("archive", project=first) == {"ok": True, "state": "archived"}
    assert act("archive-project", project=first) == {"ok": True, "state": "archived"}
    blocked(first)
    assert "Unarchive" in fails("resume", project=first)
    assert roots() == [] and index() == {}
    snap = stat()
    assert [(p["id"], p["state"]) for p in snap["projects"]] == [(first, "archived")] and snap["current"] is None, snap
    print("Verified archiving a paused Captain blocks new work and frees its root.", flush=True)

    # A new Captain on the same root starts beside the archived one.
    second = act("init", objective="Ship Alpha again")["id"]
    assert second != first
    wait(lambda: db().execute("select 1 from jobs where project=? and purpose='brief' and status='done'", (second,)).fetchone(), what="second brief")
    snap = stat()
    assert sorted((p["id"], p["state"]) for p in snap["projects"]) == sorted([(first, "archived"), (second, "active")]), snap
    assert snap["current"] == second and roots() == [root] and index() == {root: second}
    assert "Another active Captain" in fails("unarchive", project=first)
    assert state(first) == "archived"
    print("Verified a new Captain starts on an archived Captain's root and unarchive refuses while it owns the root.", flush=True)

    # Active -> archiving -> archived: running work stops first.
    crew = act("delegate", project=second, provider="claude", task="slow review")
    wait(lambda: db().execute("select pid from jobs where id=? and status='running'", (crew["job"],)).fetchone(), what="crew turn")
    worker = db().execute("select pid from jobs where id=?", (crew["job"],)).fetchone()[0]
    archived = json.loads(run("captain", "archive", second).stdout)
    assert archived["state"] in ("archiving", "archived"), archived
    wait(lambda: (stat(), state(second))[1] == "archived", what="archived")
    assert not alive(worker), "the crew turn's worker still runs"
    assert db().execute("select status from jobs where id=?", (crew["job"],)).fetchone()[0] == "stopped"
    blocked(second)
    assert roots() == [] and index() == {}
    print("Verified archiving an active Captain stops its running work first.", flush=True)

    # Unarchive succeeds once no other Captain owns the root, and work starts again.
    assert act("unarchive", project=first) == {"ok": True, "state": "active"}
    assert act("unarchive-project", project=first) == {"ok": True, "state": "active"}
    assert roots() == [root] and index() == {root: first} and stat()["current"] == first
    job = act("lead", project=first, text="Status please")["job"]
    wait(lambda: db().execute("select 1 from jobs where id=? and status='done'", (job,)).fetchone(), what="lead after unarchive")
    assert "Another active Captain" in fails("unarchive", project=second)
    assert json.loads(run("captain", "delete", first).stdout)["state"] == "deleted"
    assert json.loads(run("captain", "unarchive", second).stdout)["state"] == "active"
    assert roots() == [root] and index() == {root: second}
    assert act("unarchive", project=second) == {"ok": True, "state": "active"}
    print("Verified unarchive restores a Captain once its root is free.", flush=True)
finally:
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as c:
            c.execute("delete from watches")
    subprocess.run(["pkill", "-f", str(temp)], check=False)
