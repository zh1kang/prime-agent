"""Local project Captains. SQLite owns membership, assignments, and report delivery."""

import json
import os
import sqlite3
import subprocess
import sys
import time
import datetime
import tempfile
import functools
import uuid
from contextlib import contextmanager
from pathlib import Path

PA = os.path.abspath(sys.argv[1])
_agent = os.getenv("PRIME_AGENT_CODING_AGENT_DIR")
_default_native = (Path.home() / ".prime/agent").resolve()
_default_hub = (Path.home() / ".prime/harness").resolve()
_custom = Path(_agent).expanduser().resolve() if _agent else None
if _custom in (_default_hub, _default_native):
    _custom = None
NATIVE = (
    Path(os.getenv("PA_CAPTAIN_NATIVE_HOME") or _custom or _default_native)
    .expanduser()
    .resolve()
)
HUB = (
    Path(os.getenv("PA_CAPTAIN_HOME") or _custom or _default_hub).expanduser().resolve()
)
BASE = HUB / "captains"
STORE = BASE / "projects.sqlite3"
SESSION = os.environ.get("PA_CAPTAIN_SESSION")
_git_env = {
    k: v
    for k, v in os.environ.items()
    if k
    not in (
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
        "GIT_INDEX_FILE",
    )
}
_schema_ready = False


def actor_kind():
    """Who runs this command: user, captain (the lead's turn), or crew (a crew turn or a
    factory host, marked PA_CAPTAIN_ACTOR=crew:<project>)."""
    value = os.getenv("PA_CAPTAIN_ACTOR") or ""
    if value.startswith("crew:"):
        return "crew"
    return "captain" if value else "user"


def git_at(root, *args):
    return subprocess.run(
        ["git", "-C", str(root), *args],
        env=_git_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


@functools.lru_cache(maxsize=32)
def project_root(path):
    cwd = Path(path).expanduser().resolve(strict=True)
    if not cwd.is_dir():
        raise ValueError("Choose a project directory.")
    result = git_at(cwd, "worktree", "list", "--porcelain", "-z")
    first = result.stdout.split(b"\0", 1)[0]
    if result.returncode == 0 and first.startswith(b"worktree "):
        return str(Path(os.fsdecode(first[9:])).resolve())
    return str(cwd)


@contextmanager
def database(write=False):
    global _schema_ready
    BASE.mkdir(mode=0o700, parents=True, exist_ok=True)
    db = sqlite3.connect(STORE, timeout=10)
    db.row_factory = sqlite3.Row
    try:
        db.execute("PRAGMA foreign_keys=ON")
        if not _schema_ready:
            db.executescript("""
                CREATE TABLE IF NOT EXISTS projects (
                    id TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
                    objective TEXT NOT NULL, rules TEXT NOT NULL DEFAULT '', plan TEXT NOT NULL DEFAULT '',
                    crew_rules TEXT NOT NULL DEFAULT '', lead TEXT NOT NULL UNIQUE, parallel INTEGER NOT NULL DEFAULT 2);
                CREATE TABLE IF NOT EXISTS crew (
                    id TEXT PRIMARY KEY, project TEXT NOT NULL REFERENCES projects(id),
                    kind TEXT NOT NULL, ref TEXT NOT NULL, title TEXT NOT NULL, cwd TEXT NOT NULL,
                    provider TEXT NOT NULL, state TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
                    updated REAL NOT NULL, UNIQUE(kind,ref));
                CREATE TABLE IF NOT EXISTS jobs (
                    id TEXT PRIMARY KEY, project TEXT NOT NULL REFERENCES projects(id), member TEXT NOT NULL,
                    purpose TEXT NOT NULL, body TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                    pid INTEGER, initial_count INTEGER, dispatch TEXT, result TEXT, origin TEXT UNIQUE, base_ref TEXT, request TEXT, adapter TEXT);
                CREATE INDEX IF NOT EXISTS jobs_by_member ON jobs(member,status,created);
                CREATE TABLE IF NOT EXISTS watches (
                    project TEXT NOT NULL REFERENCES projects(id), session TEXT NOT NULL, file TEXT NOT NULL,
                    offset INTEGER NOT NULL, PRIMARY KEY(project,session,file));
                CREATE TABLE IF NOT EXISTS machines (
                    member TEXT PRIMARY KEY REFERENCES crew(id) ON DELETE CASCADE,
                    owner TEXT NOT NULL, state TEXT NOT NULL, children TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS answers (
                    member TEXT NOT NULL REFERENCES crew(id) ON DELETE CASCADE, pause INTEGER NOT NULL,
                    request TEXT NOT NULL, decision TEXT NOT NULL, text TEXT NOT NULL, stamp REAL NOT NULL,
                    PRIMARY KEY(member,pause), UNIQUE(member,request));
                CREATE TABLE IF NOT EXISTS reports (
                    project TEXT NOT NULL REFERENCES projects(id), id TEXT NOT NULL, member TEXT NOT NULL,
                    state TEXT NOT NULL, detail TEXT NOT NULL, stamp REAL NOT NULL, PRIMARY KEY(project,id));
                CREATE TABLE IF NOT EXISTS edits (
                    project TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE, field TEXT NOT NULL,
                    old_len INTEGER NOT NULL, new_len INTEGER NOT NULL, actor TEXT NOT NULL, stamp REAL NOT NULL);
                CREATE INDEX IF NOT EXISTS edits_by_project ON edits(project,stamp);
                -- The digest of the project factory version the user approved; see captain_factories.py.
                CREATE TABLE IF NOT EXISTS factory_approvals (
                    project TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE, name TEXT NOT NULL,
                    digest TEXT NOT NULL, actor TEXT NOT NULL, stamp REAL NOT NULL, PRIMARY KEY(project,name));
            """)
            db.execute("BEGIN IMMEDIATE")
            if "base_ref" not in {
                r["name"] for r in db.execute("PRAGMA table_info(jobs)")
            }:
                db.execute("ALTER TABLE jobs ADD COLUMN base_ref TEXT")
            if "parallel" not in {
                r["name"] for r in db.execute("PRAGMA table_info(projects)")
            }:
                db.execute(
                    "ALTER TABLE projects ADD COLUMN parallel INTEGER NOT NULL DEFAULT 2"
                )
            for column, ddl in (
                ("request", "TEXT"),
                ("adapter", "TEXT"),
                ("model", "TEXT"),
                ("thinking", "TEXT"),
            ):
                if column not in {
                    r["name"] for r in db.execute("PRAGMA table_info(jobs)")
                }:
                    db.execute(f"ALTER TABLE jobs ADD COLUMN {column} {ddl}")
            machine_columns = {
                r["name"] for r in db.execute("PRAGMA table_info(machines)")
            }
            # A Captain-started run records its job; the job worker owns its host.
            for column, ddl in (
                ("job", "TEXT"),
                ("resumes", "INTEGER NOT NULL DEFAULT 0"),
            ):
                if column not in machine_columns:
                    db.execute(f"ALTER TABLE machines ADD COLUMN {column} {ddl}")
            project_columns = {
                r["name"] for r in db.execute("PRAGMA table_info(projects)")
            }
            for column, ddl in (
                ("digest", "TEXT NOT NULL DEFAULT ''"),
                ("provider", "TEXT NOT NULL DEFAULT 'prime'"),
                ("model", "TEXT NOT NULL DEFAULT ''"),
                ("thinking", "TEXT NOT NULL DEFAULT ''"),
                # Writer machines the user allowed the Captain to start without asking.
                ("grants", "TEXT NOT NULL DEFAULT '[]'"),
                # active, stopping, paused, or deleting; see PROJECT_STATES.
                ("state", "TEXT NOT NULL DEFAULT 'active'"),
            ):
                if column not in project_columns:
                    db.execute(f"ALTER TABLE projects ADD COLUMN {column} {ddl}")
            db.commit()
            _schema_ready = True
        if write:
            db.execute("BEGIN IMMEDIATE")
        yield db
        db.commit()
    except BaseException:
        db.rollback()
        raise
    finally:
        db.close()


def record(db, table, key):
    if table not in ("projects", "crew", "jobs"):
        raise ValueError("Unknown record type.")
    row = db.execute("SELECT * FROM " + table + " WHERE id=?", (key,)).fetchone()
    if row is None:
        raise ValueError("That project, thread, or assignment no longer exists.")
    return dict(row)


def checked_text(text, limit=16000):
    if not isinstance(text, str) or not text.strip() or len(text.encode()) > limit:
        raise ValueError(f"Enter text between 1 and {limit} bytes.")
    return text.strip()


def thread_env(**extra):
    inherited = os.getenv("PRIME_AGENT_DAEMON_SOCKET")
    native_socket = os.getenv("PA_CAPTAIN_NATIVE_SOCKET") or (
        inherited
        if inherited and Path(inherited).parent != HUB
        else str(NATIVE / "harness-daemon.sock")
    )
    return {
        **{
            k: v
            for k, v in os.environ.items()
            if k not in ("PA_THREAD_MODEL", "PA_THREAD_THINKING")
        },
        "PRIME_AGENT_CODING_AGENT_DIR": str(HUB),
        "PRIME_AGENT_DAEMON_SOCKET": str(HUB / "daemon.sock"),
        "PA_CAPTAIN_NATIVE_SOCKET": native_socket,
        "PA_CAPTAIN_HOME": str(HUB),
        "PA_CAPTAIN_NATIVE_HOME": str(NATIVE),
        **extra,
    }


def thread_call(root, *args, interactive=False, env=None):
    run = subprocess.run(
        [PA, "thread", *args],
        cwd=root,
        env=env or thread_env(),
        text=True,
        capture_output=not interactive,
    )
    if run.returncode:
        raise ValueError(
            (
                run.stderr or run.stdout or f"Command exited with {run.returncode}"
            ).strip()[-1600:]
        )
    return (run.stdout or "").strip()


def durable(tid):
    tid = str(uuid.UUID(tid))
    return json.loads((HUB / "threads" / (tid + ".json")).read_bytes())


def native_header(path):
    path = Path(path).expanduser().resolve(strict=True)
    with path.open("rb") as stream:
        header = json.loads(stream.readline(1024 * 1024))
    if header.get("type") != "session" or not header.get("cwd") or not header.get("id"):
        raise ValueError("Choose a native Prime session or a durable thread ID.")
    return path, header


def watch_native(db, project, path, include_chat):
    path, header = native_header(path)
    directory = (
        path.parent.parent
        / "session-artifacts"
        / str(uuid.UUID(header["id"]))
        / "captain"
    )
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    journal = directory / "runs.jsonl"
    files = [journal, path] if include_chat else [journal]
    for file in files:
        db.execute(
            "INSERT OR IGNORE INTO watches VALUES(?,?,?,?)",
            (
                project,
                str(path),
                str(file),
                file.stat().st_size if file == path and file.exists() else 0,
            ),
        )


def handoff(intent, value="", **fields):
    """A target for pa to open: a chat session, a draft, or the factory page."""
    return {"handoff": dict(action=intent, value=value, **fields)}


def native_path(value):
    """The session file of a native Prime session given its path or its session ID."""
    if not value:
        return None
    path = Path(value).expanduser()
    if path.is_absolute():
        return str(path) if path.is_file() else None
    sessions = Path(os.getenv("PRIME_AGENT_SESSION_DIR") or NATIVE / "sessions")
    for found in sessions.rglob("*" + value + "*.jsonl"):
        try:
            if native_header(found)[1]["id"] == value:
                return str(found.resolve())
        except (ValueError, OSError, KeyError):
            continue
    return None


def thread_session(tid):
    """The native session a pa chat opens for a durable Prime thread, or None."""
    try:
        thread = durable(tid)
    except (FileNotFoundError, ValueError):
        return None
    if thread.get("provider") != "prime":
        return None
    return native_path(thread.get("native"))


def member_session(member):
    if member["kind"] == "native":
        return member["ref"]
    if member["kind"] == "factory":
        return member["ref"].rsplit("#", 1)[0]
    return thread_session(member["id"])


def snapshot():
    reconcile()
    here = project_root(Path.cwd())
    with database() as db:
        projects = [
            dict(row)
            for row in db.execute(
                "SELECT *,rowid AS indicator FROM projects ORDER BY name COLLATE NOCASE,root"
            )
        ]
        watching = bool(db.execute("SELECT 1 FROM watches LIMIT 1").fetchone())
        for project in projects:
            project["crew"] = [
                dict(row)
                for row in db.execute(
                    "SELECT c.*,m.state AS run_state,m.job AS run_job FROM crew c LEFT JOIN machines m ON m.member=c.id WHERE c.project=? ORDER BY c.updated DESC,c.id",
                    (project["id"],),
                )
            ]
            busy = {
                r[0]
                for r in db.execute(
                    "SELECT member FROM jobs WHERE project=? AND status IN ('queued','running')",
                    (project["id"],),
                )
            }
            project["lead_session"] = thread_session(project["lead"])
            project["lead_busy"] = project["lead"] in busy
            for member in project["crew"]:
                if member["kind"] == "factory":
                    member.update(run_summary(member))
                member["session"] = member_session(member)
                member["busy"] = member["id"] in busy
            latest = db.execute(
                "SELECT status,result FROM jobs WHERE member=? ORDER BY created DESC LIMIT 1",
                (project["lead"],),
            ).fetchone()
            project["lead_state"] = (
                "running"
                if latest and latest[0] in ("queued", "running")
                else "needs_you"
                if latest and latest[0] in ("needs_you", "interrupted")
                else "ready"
            )
            project["lead_detail"] = latest[1] if latest else ""
            project["edits"] = [
                dict(r)
                for r in db.execute(
                    "SELECT field,old_len,new_len,actor,stamp FROM edits WHERE project=? ORDER BY stamp DESC LIMIT 10",
                    (project["id"],),
                )
            ]
            project["reports"] = [
                dict(r)
                for r in db.execute(
                    "SELECT * FROM reports WHERE project=? ORDER BY stamp DESC LIMIT 20",
                    (project["id"],),
                )
            ]
    for project in projects:
        try:
            project["factories"] = project_factories(project)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            # The panel shows the reason; the rest of the snapshot stays usable.
            project["factories"] = []
            project["factories_error"] = str(error)[:400]
    if not (BASE / "roots.json").is_file():
        write_roots()
    if watching:
        start_watch()
    return {
        "current_root": here,
        "current": next((p["id"] for p in projects if p["root"] == here), None),
        "projects": projects,
    }


def action(op, project=None, **args):
    if op in ("init", "promote"):
        return create_project(op, args)
    if op == "choices":
        return choices(project, args)
    if op == "launch":
        return handoff("draft", launch_draft(args.get("machine"), args.get("params") or {}))
    if op == "factory" and project is None:
        with database() as db:
            saved = db.execute(
                "SELECT id FROM projects WHERE root=?", (project_root(Path.cwd()),)
            ).fetchone()
        if not saved:
            return handoff("factory")
        project = saved[0]
    with database() as db:
        if project is None:
            found = db.execute(
                "SELECT id FROM projects WHERE root=?", (project_root(Path.cwd()),)
            ).fetchone()
            if not found:
                raise ValueError("Initialize a Captain for this project first.")
            project = found[0]
        p = record(db, "projects", project)
    member = args.get("member")
    m = None
    if member and member != "lead":
        with database() as db:
            m = record(db, "crew", member)
        if m["project"] != project:
            raise ValueError("Select a member of this project.")
        if m["kind"] == "thread" and op in ("open", "review", "reply", "recover"):
            actual = durable(member)
            if project_root(actual["cwd"]) != p["root"]:
                raise ValueError(
                    "This worker's current checkout no longer belongs to its Captain."
                )
            m.update(cwd=actual["cwd"], provider=actual["provider"])
            with database(True) as db:
                db.execute(
                    "UPDATE crew SET cwd=?,provider=? WHERE id=? AND project=?",
                    (m["cwd"], m["provider"], member, project),
                )
    if op == "edit":
        field = args["field"]
        actor = actor_kind()
        if actor == "crew" or field not in USER_FIELDS or (actor == "captain" and field not in LEAD_FIELDS):
            raise ValueError("Unknown Captain setting.")
        if field == "model":
            provider, value = checked_model(
                args.get("provider") or p["provider"], args.get("value")
            )
        elif field == "thinking":
            value = checked_thinking(args.get("value"))
        elif field == "parallel":
            value = str(checked_parallel(args["value"]))
        else:
            value = (
                checked_text(str(args["value"]))
                if field in ("objective", "name") or args["value"]
                else ""
            )
        with database(True) as db:
            if field == "model":
                if provider != p["provider"] and db.execute(
                    "SELECT 1 FROM jobs WHERE member=? AND status IN ('queued','running')",
                    (p["lead"],),
                ).fetchone():
                    raise ValueError(
                        "Wait for the Captain's turn to finish before changing its provider."
                    )
                db.execute(
                    "UPDATE projects SET provider=?,model=? WHERE id=?",
                    (provider, value, project),
                )
                old = p["provider"] + " " + p["model"]
                value = provider + " " + value
            else:
                old = str(p[field])
                db.execute(
                    "UPDATE projects SET " + field + "=? WHERE id=?", (value, project)
                )
            db.execute(
                "INSERT INTO edits VALUES(?,?,?,?,?,?)",
                (project, field, len(old), len(value), actor, time.time()),
            )
        return {"ok": True, "field": field, "by": actor}
    elif op in ("stop", "resume", "delete", "demote"):
        return project_state_action(op, p)
    elif op == "adopt":
        ref = args["ref"]
        if ref == "current":
            ref = SESSION or ""
            if not ref:
                raise ValueError("Open a saved conversation in pa to adopt it.")
        if Path(ref).expanduser().is_file():
            path, h = native_header(ref)
            ref, cwd, kind, provider = str(path), h["cwd"], "native", "prime"
            title = args.get("title") or native_title(path) or "Prime conversation"
            mid = str(uuid.UUID(h["id"]))
        else:
            t = durable(ref)
            if t.get("parent"):
                raise ValueError(
                    "Adopt a root thread. Subthreads stay with their owner."
                )
            ref = mid = t["id"]
            cwd, kind, provider = t["cwd"], "thread", t["provider"]
            title = args.get("title") or next(
                (s for role, s in t["messages"] if role == "user"), "Provider thread"
            )
        if project_root(cwd) != p["root"]:
            raise ValueError("This thread belongs to another project.")
        with database(True) as db:
            if db.execute("SELECT 1 FROM projects WHERE lead=?", (mid,)).fetchone():
                raise ValueError("A Captain cannot join a crew.")
            prior = db.execute(
                "SELECT project,ref FROM crew WHERE id=?", (mid,)
            ).fetchone()
            if prior and (prior[0] != project or prior[1] != ref):
                raise ValueError("This thread already belongs to a crew.")
            db.execute(
                "INSERT OR IGNORE INTO crew VALUES(?,?,?,?,?,?,?,?,?,?)",
                (
                    mid,
                    project,
                    kind,
                    ref,
                    title[:180],
                    cwd,
                    provider,
                    "ready",
                    "",
                    time.time(),
                ),
            )
            if kind == "native":
                watch_native(db, project, ref, True)
            else:
                file = HUB / "threads" / (mid + ".json")
                db.execute(
                    "INSERT OR IGNORE INTO watches VALUES(?,?,?,?)",
                    (project, ref, str(file), len(t["messages"])),
                )
        start_watch()
        return {"id": mid}
    elif op == "release" and m:
        if m["kind"] == "factory":
            raise ValueError(
                "Release applies to crew threads. Use the live factory to control this run."
            )
        with database(True) as db:
            active = db.execute(
                "SELECT 1 FROM jobs WHERE member=? AND status IN ('queued','running')",
                (member,),
            ).fetchone()
            if active:
                raise ValueError(
                    "Wait for the active assignment before releasing its thread."
                )
            db.execute("DELETE FROM crew WHERE id=? AND project=?", (member, project))
            if m["kind"] in ("native", "thread"):
                db.execute(
                    "DELETE FROM watches WHERE project=? AND session=?",
                    (project, m["ref"]),
                )
    elif op in ("lead", "reply", "delegate"):
        require_active(p)
        return dispatch_action(p, op, m, args)
    elif op == "grant":
        return grant_machine(p, args)
    elif op.startswith("factory-"):
        return factory_action(op, p, args)
    elif op == "answer":
        if not m or m["kind"] != "factory":
            raise ValueError("Choose a factory run to answer.")
        return answer_run(p, m, args)
    elif op in ("factory", "consensus"):
        require_active(p)
        local = project_root(Path.cwd()) == p["root"]
        session = Path(SESSION) if local and SESSION else None
        if not session or not session.is_file():
            session = factory_session(p)
            local = False
        with database(True) as db:
            watch_native(db, project, session, False)
        start_watch()
        if op == "factory" and args.get("machine"):
            draft = factory_draft(args["machine"], p["objective"])
            return (
                handoff("draft", draft)
                if local
                else handoff("resume", str(session), draft=draft)
            )
        if op == "factory":
            return (
                handoff("factory")
                if local
                else handoff("resume", str(session), panel="factory")
            )
        task = checked_text(args["task"])
        draft = f"Use the dual-consensus factory for this decision. In the Python kernel call await rlm.factory.run('dual-consensus', params={{'task': {task!r}}}). Report the agreed decision or the unresolved votes."
        return (
            handoff("draft", draft)
            if local
            else handoff("resume", str(session), draft=draft)
        )
    elif op == "review" and m:
        # pa's review panel for a crew checkout: one review request in, its JSON answer out.
        request = args.get("request")
        if not isinstance(request, dict):
            raise ValueError("Send the review request as an object.")
        run = subprocess.run(
            [PA, "thread", "review", "json"],
            cwd=m["cwd"],
            env=thread_env(),
            input=json.dumps(request),
            text=True,
            capture_output=True,
        )
        if run.returncode:
            raise ValueError((run.stderr or run.stdout or "Review failed.").strip()[-1600:])
        return json.loads(run.stdout)
    elif op == "open":
        if m and m["kind"] == "factory" and run_control(m["id"]) is not None:
            raise ValueError("This run has no chat session. Open it from the factory page.")
        if not m and member != "lead":
            raise ValueError("Select a thread to open.")
        if m:
            session = member_session(m)
            title = m["title"]
        else:
            ensure_lead(p)
            session = thread_session(p["lead"])
            title = "captain"
        if session:
            return handoff(
                "resume",
                session,
                **({"panel": "factory"} if m and m["kind"] == "factory" else {}),
            )
        tid = m["id"] if m else p["lead"]
        thread = durable(tid)
        return handoff("thread", tid, provider=thread["provider"], title=title)
    elif op == "transcript":
        tid = m["id"] if m else p["lead"]
        if m and m["kind"] != "thread":
            raise ValueError("Open this conversation as a pa chat.")
        thread = durable(tid)
        with database() as db:
            busy = bool(
                db.execute(
                    "SELECT 1 FROM jobs WHERE member=? AND status IN ('queued','running')",
                    (tid,),
                ).fetchone()
            )
        return {
            "id": tid,
            "provider": thread["provider"],
            "model": thread.get("model"),
            "cwd": thread["cwd"],
            "messages": thread["messages"],
            "pending": thread.get("pending"),
            "busy": busy,
        }
    elif op == "recover":
        if m and m["kind"] != "thread":
            raise ValueError("Open the native conversation to recover it.")
        tid = m["id"] if m else p["lead"]
        with database() as db:
            if db.execute(
                "SELECT 1 FROM jobs WHERE member=? AND status='running'", (tid,)
            ).fetchone():
                raise ValueError("This assignment is still running.")
        thread_call(m["cwd"] if m else p["root"], tid, "recover")
    else:
        raise ValueError("Choose a supported action and thread.")
    return {"ok": True}


USER_FIELDS = (
    "plan",
    "rules",
    "crew_rules",
    "digest",
    "objective",
    "name",
    "parallel",
    "model",
    "thinking",
)
# The Captain maintains its own records. Name, model, thinking, and limits stay with the user.
LEAD_FIELDS = ("plan", "rules", "crew_rules", "digest", "objective")
PROVIDERS = ("prime", "claude", "codex", "antigravity")
THINKING_LEVELS = ("", "off", "minimal", "low", "medium", "high", "xhigh", "max")
PROVIDER_FAMILIES = {"claude": ("anthropic",), "codex": ("openai", "openai-codex")}


def checked_thinking(value):
    level = str(value or "").strip().casefold()
    if level == "default":
        return ""
    if level not in THINKING_LEVELS:
        raise ValueError(
            "Choose a thinking level: default, off, minimal, low, medium, high, xhigh, or max."
        )
    return level


def checked_parallel(value):
    value = str(value).strip()
    if not value.isdecimal() or not 1 <= int(value) <= 128:
        raise ValueError("Choose a parallel worker limit from 1 to 128.")
    return int(value)


@functools.lru_cache(maxsize=1)
def model_catalog():
    """Rows of `pa model list` for the native home that owns model credentials."""
    run = subprocess.run(
        [PA, "model", "list"],
        env={**os.environ, "PRIME_AGENT_CODING_AGENT_DIR": str(NATIVE)},
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=60,
    )
    lines = run.stdout.splitlines() if run.returncode == 0 else []
    if not lines or lines[0].split()[:2] != ["provider", "model"]:
        return ()
    return tuple(tuple(line.split()[:2]) for line in lines[1:] if len(line.split()) >= 2)


@functools.lru_cache(maxsize=1)
def antigravity_models():
    """Base model IDs reported by `pa thread models antigravity`."""
    try:
        run = subprocess.run(
            [PA, "thread", "models", "antigravity"],
            env=thread_env(),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.SubprocessError):
        return ()
    if run.returncode != 0:
        return ()
    out = []
    for line in run.stdout.splitlines():
        parts = line.split("\t")
        if parts and parts[0].strip():
            out.append(parts[0].strip())
    return tuple(out)


def model_choices():
    """(provider, model) pairs a Captain can run; an empty model is the provider default."""
    rows = model_catalog()
    choices = [("prime", "")] + [("prime", a + "/" + b) for a, b in rows]
    for provider, families in PROVIDER_FAMILIES.items():
        choices.append((provider, ""))
        choices += [(provider, b) for a, b in rows if a in families]
    choices.append(("antigravity", ""))
    choices += [("antigravity", mid) for mid in antigravity_models()]
    return choices


def checked_model(provider, model):
    if provider not in PROVIDERS:
        raise ValueError("Choose Prime, Claude, Codex, or Antigravity.")
    model = str(model or "").strip()
    if model and (provider, model) not in model_choices():
        if provider == "antigravity":
            # Also allow explicit effort-suffixed IDs like `gemini-4-neon-high` when base is listed.
            base = model.rsplit("-", 1)[0] if "-" in model else ""
            if base and (provider, base) in model_choices():
                return provider, model
        raise ValueError(f"Unknown {provider} model {model}. See pa model list.")
    return provider, model


def write_roots():
    """Publish Captain roots so the chat screen can show a setup hint without Python."""
    with database() as db:
        roots = sorted(r[0] for r in db.execute("SELECT root FROM projects"))
    with tempfile.NamedTemporaryFile("w", dir=BASE, prefix=".roots-", delete=False) as out:
        json.dump(roots, out)
    os.replace(out.name, BASE / "roots.json")


def promote_source(ref, root):
    """Resolve an existing root thread to a durable lead thread, importing native history."""
    live = ref == "current"
    if live:
        ref = SESSION or ""
        if not ref:
            raise ValueError("Open a saved conversation in pa to promote it.")
    if Path(ref).expanduser().is_file():
        path, header = native_header(ref)
        if header.get("rlmDepth", 0):
            raise ValueError("Promote a root conversation. Subagents stay with their owner.")
        if project_root(header["cwd"]) != root:
            raise ValueError("This conversation belongs to another project.")
        # The open chat holds its native session, so the Captain starts a new one
        # with the transcript as context.
        extra = ["--context-only"] if live else []
        ref = thread_call(header["cwd"], "import", "prime", str(path), *extra).splitlines()[-1]
    thread = durable(ref)
    if thread.get("parent"):
        raise ValueError("Promote a root thread. Subthreads stay with their owner.")
    if project_root(thread["cwd"]) != root:
        raise ValueError("This thread belongs to another project.")
    return thread


def brief_prefill(ref):
    """Seed a promoted thread's brief with its first request."""
    if ref == "current":
        ref = SESSION or ""
    if ref and Path(ref).expanduser().is_file():
        first = native_title(Path(ref).expanduser())
    else:
        first = next((s for role, s in durable(ref)["messages"] if role == "user"), "")
    return (
        (first.strip() + "\n\n" if first.strip() else "")
        + "Summarize this thread's goal, decisions, and open work, then continue from there."
    )


def create_project(op, args):
    root = project_root(args.get("root") or Path.cwd())
    brief = checked_text(args.get("brief") or args.get("objective"))
    parallel = checked_parallel(args.get("parallel", 2))
    crew = args.get("crew") or []
    if not isinstance(crew, list) or not all(isinstance(r, str) for r in crew):
        raise ValueError("Pass crew as a list of thread references.")
    thread = promote_source(args["ref"], root) if op == "promote" else None
    provider, model = checked_model(
        args.get("provider") or (thread["provider"] if thread else "prime"),
        args.get("model"),
    )
    thinking = checked_thinking(args.get("thinking"))
    lead = thread["id"] if thread else str(uuid.uuid4())
    with database(True) as db:
        existing = db.execute("SELECT * FROM projects WHERE root=?", (root,)).fetchone()
        if existing and op == "promote" and existing["lead"] != lead:
            raise ValueError("This project already has a Captain. Demote it first.")
        if existing:
            p = dict(existing)
        else:
            if db.execute("SELECT 1 FROM crew WHERE id=?", (lead,)).fetchone():
                raise ValueError("Release this thread from its crew before promoting it.")
            if db.execute("SELECT 1 FROM projects WHERE lead=?", (lead,)).fetchone():
                raise ValueError("This thread is already a Captain.")
            p = dict(
                id=str(uuid.uuid4()),
                root=root,
                name=Path(root).name or root,
                objective=brief,
                lead=lead,
                parallel=parallel,
                provider=provider,
                model=model,
                thinking=thinking,
            )
            db.execute(
                "INSERT INTO projects(id,root,name,objective,lead,parallel,provider,model,thinking) VALUES(?,?,?,?,?,?,?,?,?)",
                tuple(p.values()),
            )
        # A new project, or one whose creation stopped before its first turn, gets the brief.
        first_turn = not existing or not db.execute(
            "SELECT 1 FROM jobs WHERE member=?", (p["lead"],)
        ).fetchone()
    write_roots()
    if not thread and not existing:
        # An existing lead may be mid-turn; its next job aligns it with ensure_lead.
        thread_call(root, "ensure", p["lead"], p["provider"])
    errors = []
    for ref in crew:
        try:
            action("adopt", project=p["id"], ref=ref)
        except (ValueError, OSError, KeyError) as error:
            errors.append(f"{ref}: {error}")
    result = {"id": p["id"], "lead": p["lead"]}
    if errors:
        result["crew_errors"] = errors
    if first_turn:
        with database(True) as db:
            result["job"] = queue(
                db, p["id"], p["lead"], "brief", p["objective"], "brief/" + p["id"]
            )
        launch_worker("run-job", result["job"])
    if args.get("design") is True:
        # The design request runs after the brief turn that writes the plan.
        with database(True) as db:
            result["design_job"] = queue(
                db, p["id"], p["lead"], "design", DESIGN_REQUEST, "design/" + p["id"] + "/setup"
            )
        launch_worker("run-job", result["design_job"])
    if args.get("machine"):
        # A starting factory opens drafted in the project's factory chat.
        try:
            result.update(action("factory", project=p["id"], machine=args["machine"]))
        except (ValueError, OSError, KeyError) as error:
            result["factory_error"] = str(error)
    return result


def native_title(path):
    with path.open("rb") as stream:
        for raw in stream.read(65536).splitlines():
            try:
                event = json.loads(raw)
            except (ValueError, UnicodeError):
                continue
            msg = event.get("message", {})
            if msg.get("role") == "user":
                return message_text(msg)[:180]
    return ""


def message_text(msg):
    content = msg.get("content", "")
    return (
        content
        if isinstance(content, str)
        else " ".join(
            x.get("text", "")
            for x in (content or [])
            if isinstance(x, dict) and x.get("type") == "text"
        )
    )


def choices(project, args):
    """Options for pa's Captain forms: models, thinking levels, machines, threads to add."""
    if project:
        with database() as db:
            p = record(db, "projects", project)
    else:
        p = {"root": project_root(args.get("root") or Path.cwd())}
    result = {
        "models": [{"provider": a, "model": b} for a, b in model_choices()],
        "crew_models": crew_models(),
        "thinking": [level or "default" for level in THINKING_LEVELS],
        "machines": factory_choices(),
        "machine_params": {name: launch_params(name) for name in factory_choices()},
        "adopt": [
            {"ref": ref, "label": label} for ref, label, _ in adoption_options(p)
        ],
    }
    if project:
        # Approved project factories can be assigned; drafts wait for review on the F page.
        result["project_factories"] = [
            f["name"] for f in project_factories(p) if f["state"] == "approved"
        ]
    if args.get("ref"):
        result["brief"] = brief_prefill(args["ref"])
    return result


# The pa model a provider choice maps to when the Captain starts a crew thread. Each runs in a
# native Prime session, so pa opens it as an ordinary chat.
CREW_DEFAULTS = (
    ("anthropic", ("claude-opus-5-5",)),
    ("openai-codex", ("gpt-5.3-codex", "gpt-5.2-codex")),
    ("antigravity", ("gemini-4-neon",)),
)


def crew_models():
    """(label, model) rows for a new crew thread: Prime's default first, then one per family."""
    rows = model_catalog()
    out = [{"label": "prime · default model", "provider": "prime", "model": ""}]
    for family, preferred in CREW_DEFAULTS:
        names = [b for a, b in rows if a == family]
        pick = next((n for n in preferred if n in names), names[0] if names else None)
        if pick:
            out.append({"label": family + " · " + pick, "provider": "prime", "model": family + "/" + pick})
    seen = {row["model"] for row in out}
    out += [
        {"label": a + " · " + b, "provider": "prime", "model": a + "/" + b}
        for a, b in rows
        if a + "/" + b not in seen
    ]
    return out


@functools.lru_cache(maxsize=1)
def installed_machines():
    """Machines installed in the native home and the current environment, read once per call."""
    return {**machine_catalog(PA, NATIVE), **machine_catalog(PA)}


def factory_choices():
    """Installed machines, with the two onboarding defaults first."""
    names = list(installed_machines())
    first = [n for n in ("project-builder", "dual-consensus") if n in names]
    return first + [n for n in names if n not in first]


def machine_fields(machine):
    machines = installed_machines()
    if machine not in machines:
        raise ValueError("Choose an installed machine.")
    path = Path(machines[machine]["path"])
    if path.name != "machine.toml":
        return {}
    import tomllib

    with path.open("rb") as stream:
        return tomllib.load(stream).get("params", {})


def launch_params(machine):
    """A machine's run parameters, for pa's launch form."""
    try:
        fields = machine_fields(machine)
    except (ValueError, OSError) as error:
        return [{"error": str(error)}]
    return [
        {
            "name": name,
            "type": spec.get("type", "text"),
            "required": "default" not in spec,
            "description": spec.get("description", ""),
        }
        for name, spec in fields.items()
    ]


def launch_draft(machine, values):
    """The chat prompt that starts `machine` with `values`: text stays text, other types are JSON."""
    if not isinstance(values, dict):
        raise ValueError("Pass params as an object.")
    fields = machine_fields(machine)
    params = {}
    for name, spec in fields.items():
        value = str(values.get(name) or "").strip()
        if not value:
            if "default" not in spec:
                raise ValueError(f"{name} requires a value.")
            continue
        if spec.get("type", "text") == "text":
            params[name] = value
        else:
            try:
                params[name] = json.loads(value)
            except ValueError:
                raise ValueError(f"Enter a JSON value for {name}.") from None
    extra = f", params={params!r}" if params else ""
    return f"Start the {machine} factory in this project. In the Python kernel, call await rlm.factory.run({machine!r}{extra}) once and report the run ID."


def factory_draft(machine, brief):
    machines = installed_machines()
    if machine not in machines:
        raise ValueError("Choose an installed machine.")
    params = {}
    path = Path(machines[machine]["path"])
    if path.name == "machine.toml":
        import tomllib

        with path.open("rb") as stream:
            fields = tomllib.load(stream).get("params", {})
        # The brief fills the first required text parameter, such as a task.
        field = next(
            (
                name
                for name, spec in fields.items()
                if spec.get("type") == "text" and "default" not in spec
            ),
            None,
        )
        if field:
            params[field] = brief
    extra = f", params={params!r}" if params else ""
    return f"Start the {machine} factory for this project. In the Python kernel, call await rlm.factory.run({machine!r}{extra}) once and report the run ID."


def factory_session(project):
    sid = str(uuid.uuid5(uuid.UUID(project["id"]), "factory"))
    root = Path(os.getenv("PRIME_AGENT_SESSION_DIR") or NATIVE / "sessions")
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    path = root / (sid + ".jsonl")
    if not path.exists():
        with tempfile.NamedTemporaryFile(
            "w", dir=root, prefix=".captain-", delete=False
        ) as draft:
            try:
                print(
                    json.dumps(
                        dict(
                            type="session",
                            version=3,
                            id=sid,
                            timestamp=datetime.datetime.now(
                                datetime.timezone.utc
                            ).isoformat(),
                            cwd=project["root"],
                        )
                    ),
                    file=draft,
                )
                draft.flush()
                os.fsync(draft.fileno())
                try:
                    os.link(draft.name, path)
                except FileExistsError:
                    pass
            finally:
                os.unlink(draft.name)
    _, header = native_header(path)
    if header["id"] != sid or project_root(header["cwd"]) != project["root"]:
        raise ValueError("Factory session does not match the project.")
    return path


def adoption_options(project):
    with database() as db:
        owned = {
            r[0]
            for r in db.execute("SELECT id FROM crew UNION SELECT lead FROM projects")
        }
    options = []
    sessions = Path(os.getenv("PRIME_AGENT_SESSION_DIR") or NATIVE / "sessions")
    for path in sessions.glob("*.jsonl"):
        try:
            path, header = native_header(path)
            if (
                header["id"] in owned
                or header.get("rlmDepth", 0)
                or project_root(header["cwd"]) != project["root"]
            ):
                continue
            title = native_title(path) or "Prime conversation"
            options.append(
                (
                    str(path),
                    ("Current · " if SESSION and path == Path(SESSION) else "Prime · ")
                    + title,
                    path.stat().st_mtime,
                )
            )
        except (ValueError, OSError, KeyError):
            continue
    for path in (HUB / "threads").glob("*.json"):
        try:
            item = durable(path.stem)
            if (
                item["id"] in owned
                or item.get("parent")
                or project_root(item["cwd"]) != project["root"]
            ):
                continue
            title = next(
                (s for role, s in item["messages"] if role == "user"), "Provider thread"
            )
            options.append(
                (
                    item["id"],
                    item["provider"].capitalize() + " · " + title,
                    path.stat().st_mtime,
                )
            )
        except (ValueError, OSError, KeyError):
            continue
    return sorted(options, key=lambda opt: (opt[0] != SESSION, -opt[2]))
