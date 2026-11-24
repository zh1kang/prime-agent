"""Local project Captains. SQLite owns membership, assignments, and report delivery."""

import fcntl
import hashlib
import json
import os
import re
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


def session_actor():
    """The actor of the session whose kernel runs this command, from RLM_SESSION_ID, which the
    daemon sets for every session's tools: the project of a Captain lead session, crew:<project>
    for a crew session, or "" for any other session. A subagent session has the actor of the
    session that spawned it. The daemon may run a Captain turn in a worker that a pa chat
    started, so the environment of that worker cannot carry the actor."""
    sid = os.getenv("RLM_SESSION_ID") or ""
    try:
        sid = str(uuid.UUID(sid))
    except ValueError:
        return ""
    if not STORE.exists():
        return ""
    owners = {}
    with database() as db:
        for p in db.execute("SELECT id,lead FROM projects"):
            owners[thread_session(p["lead"])] = p["id"]
        for m in db.execute("SELECT * FROM crew"):
            # A factory run that a chat watches lives in that chat; only a Captain-started
            # run, whose ref is its host session, makes its session crew.
            if m["kind"] == "factory" and "#" in m["ref"]:
                continue
            owners.setdefault(member_session(dict(m)), "crew:" + m["project"])
    owners.pop(None, None)
    ids = {}
    for path, actor in owners.items():
        try:
            ids[str(uuid.UUID(native_header(path)[1]["id"]))] = actor
        except (ValueError, OSError, KeyError, TypeError):
            continue
    path = None
    for _ in range(16):
        if sid in ids:
            return ids[sid]
        if path is None:
            # A subagent's session lives under its parent's artifacts.
            path = native_path(sid) or next(
                (NATIVE / "session-artifacts").glob("*/sub-*/" + sid + ".jsonl"), None
            )
        if not path:
            return ""
        try:
            child = native_header(path)[1]
            # A user /fork starts a normal chat; only an RLM subagent keeps its parent's actor.
            parent = child.get("parentSession") if child.get("rlmDepth", 0) else None
            path, header = native_header(parent) if parent else (None, None)
        except (ValueError, OSError):
            return ""
        if not header:
            return ""
        sid = str(header["id"])
    return ""


def effective_actor():
    """The actor that runs this command: the stricter of PA_CAPTAIN_ACTOR and the session's actor,
    so a Captain or crew turn never counts as the user."""
    env = os.getenv("PA_CAPTAIN_ACTOR") or ""
    session = session_actor()
    if env and session and env.removeprefix("crew:") != session.removeprefix("crew:"):
        raise ValueError("This thread belongs to a different Captain.")
    if env.startswith("crew:") or session.startswith("crew:"):
        return env if env.startswith("crew:") else session
    return env or session


def actor_kind():
    """Who runs this command: user, captain (the lead's turn), or crew (a crew turn or a
    factory host, marked PA_CAPTAIN_ACTOR=crew:<project>, or a crew session)."""
    value = effective_actor()
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


def fsync_dir(directory):
    try:
        fd = os.open(directory, os.O_RDONLY)
    except OSError:
        return
    try:
        os.fsync(fd)
    except OSError:
        pass
    finally:
        os.close(fd)


def atomic_write_text(path, text, mode=None):
    """Replace `path` durably: write a temporary file in its folder, fsync it, rename it over
    `path`, then fsync the folder. A crash leaves the old file or the new one, never a torn one."""
    path = Path(path)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    data = text.encode() if isinstance(text, str) else text
    fd, temporary = tempfile.mkstemp(dir=path.parent, prefix="." + path.name + "-", suffix=".tmp")
    try:
        with os.fdopen(fd, "wb") as out:
            out.write(data)
            out.flush()
            os.fsync(out.fileno())
        if mode is not None:
            os.chmod(temporary, mode)
        os.replace(temporary, path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise
    fsync_dir(path.parent)


def atomic_write_json(path, value):
    atomic_write_text(path, json.dumps(value, ensure_ascii=False))


def current_root():
    """The project root of this process's folder, or None when the folder is gone."""
    try:
        return project_root(Path.cwd())
    except (FileNotFoundError, ValueError):
        return None


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
        # busy_timeout first: switching to WAL takes a lock another process may hold.
        db.execute("PRAGMA busy_timeout=10000")
        db.execute("PRAGMA journal_mode=WAL")
        db.execute("PRAGMA synchronous=FULL")
        db.execute("PRAGMA foreign_keys=ON")
        if not _schema_ready:
            db.executescript("""
                CREATE TABLE IF NOT EXISTS projects (
                    id TEXT PRIMARY KEY, root TEXT NOT NULL, name TEXT NOT NULL,
                    objective TEXT NOT NULL, rules TEXT NOT NULL DEFAULT '', plan TEXT NOT NULL DEFAULT '',
                    crew_rules TEXT NOT NULL DEFAULT '', lead TEXT NOT NULL UNIQUE, parallel INTEGER NOT NULL DEFAULT 2);
                CREATE TABLE IF NOT EXISTS crew (
                    id TEXT PRIMARY KEY, project TEXT NOT NULL REFERENCES projects(id),
                    kind TEXT NOT NULL, ref TEXT NOT NULL, title TEXT NOT NULL, cwd TEXT NOT NULL,
                    provider TEXT NOT NULL, state TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
                    updated REAL NOT NULL, model TEXT NOT NULL DEFAULT '', thinking TEXT NOT NULL DEFAULT '',
                    UNIQUE(kind,ref));
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
                -- A writer machine the Captain asked to start; the user approves or declines it.
                CREATE TABLE IF NOT EXISTS pending_grants (
                    id TEXT PRIMARY KEY, project TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                    machine TEXT NOT NULL, task TEXT NOT NULL, params TEXT NOT NULL DEFAULT '{}',
                    base TEXT NOT NULL DEFAULT 'HEAD', request_id TEXT NOT NULL, created_at REAL NOT NULL,
                    UNIQUE(project,request_id));
                CREATE TABLE IF NOT EXISTS notifications (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, project TEXT NOT NULL, member TEXT NOT NULL DEFAULT '',
                    kind TEXT NOT NULL, title TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL, read_at TEXT NOT NULL DEFAULT '');
                CREATE TABLE IF NOT EXISTS decisions (
                    id TEXT PRIMARY KEY, project TEXT NOT NULL, member TEXT NOT NULL DEFAULT '',
                    title TEXT NOT NULL, summary TEXT NOT NULL, recommendation TEXT NOT NULL DEFAULT '',
                    options_json TEXT NOT NULL DEFAULT '[]', status TEXT NOT NULL DEFAULT 'open',
                    chosen_key TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL, resolved_at TEXT NOT NULL DEFAULT '');
                CREATE TABLE IF NOT EXISTS schedules (
                    id TEXT PRIMARY KEY, project TEXT NOT NULL, title TEXT NOT NULL, cron_expr TEXT NOT NULL,
                    task TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, last_run_at TEXT NOT NULL DEFAULT '',
                    next_run_at TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS review_findings (
                    id TEXT PRIMARY KEY, project TEXT NOT NULL, member TEXT NOT NULL, file TEXT NOT NULL,
                    snippet_hash TEXT NOT NULL, category TEXT NOT NULL, severity TEXT NOT NULL,
                    confidence TEXT NOT NULL DEFAULT 'Confirmed', line_hint INTEGER, message TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'open', created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
            """)
            # A table rebuild must not run ON DELETE actions or check references while the
            # old table is dropped; the pragma is a no-op inside a transaction.
            db.execute("PRAGMA foreign_keys=OFF")
            db.execute("BEGIN IMMEDIATE")
            migrate_schema(db)
            db.commit()
            db.execute("PRAGMA foreign_keys=ON")
            _schema_ready = True
        if write:
            db.execute("BEGIN IMMEDIATE")
            track_touched_projects(db)
        yield db
        touched = (
            {r[0] for r in db.execute("SELECT project FROM temp.touched") if r[0]} if write else set()
        )
        db.commit()
        if touched:
            # The write is committed; a failed status file is rewritten by the next publish.
            try:
                publish_project_statuses(db, project_ids=touched)
            except (OSError, sqlite3.Error, ValueError) as error:
                print(f"Captain: could not publish the project status: {error}", file=sys.stderr)
    except BaseException:
        db.rollback()
        raise
    finally:
        db.close()


# Columns added after a table's first release, in release order: (table, column, DDL). A
# store at an older PRAGMA user_version gets the missing ones; append only, never reorder.
SCHEMA_COLUMNS = (
    ("jobs", "base_ref", "TEXT"),
    ("projects", "parallel", "INTEGER NOT NULL DEFAULT 2"),
    ("jobs", "request", "TEXT"),
    ("jobs", "adapter", "TEXT"),
    ("jobs", "model", "TEXT"),
    ("jobs", "thinking", "TEXT"),
    # A Captain-started run records its job; the job worker owns its host.
    ("machines", "job", "TEXT"),
    ("machines", "resumes", "INTEGER NOT NULL DEFAULT 0"),
    # The native session that hosts the run and the run's ID in its kernel.
    ("machines", "session", "TEXT"),
    ("machines", "run_id", "TEXT"),
    # An answer is committed first, then delivered; applied_at marks the delivery.
    ("answers", "applied_at", "REAL"),
    # The full previous text of an edited field, so the user can restore it.
    ("edits", "old_value", "TEXT"),
    ("crew", "model", "TEXT NOT NULL DEFAULT ''"),
    ("crew", "thinking", "TEXT NOT NULL DEFAULT ''"),
    # The project specialist whose prompt, model, and thinking the thread uses.
    ("crew", "specialist", "TEXT NOT NULL DEFAULT ''"),
    # The writer's worktree folder was removed outside pa.
    ("crew", "worktree_missing", "INTEGER NOT NULL DEFAULT 0"),
    # Hidden from the crew list; the transcript and worktree stay.
    ("crew", "archived", "INTEGER NOT NULL DEFAULT 0"),
    ("projects", "digest", "TEXT NOT NULL DEFAULT ''"),
    ("projects", "provider", "TEXT NOT NULL DEFAULT 'prime'"),
    ("projects", "model", "TEXT NOT NULL DEFAULT ''"),
    ("projects", "thinking", "TEXT NOT NULL DEFAULT ''"),
    # Writer machines the user allowed the Captain to start without asking.
    ("projects", "grants", "TEXT NOT NULL DEFAULT '[]'"),
    # active, stopping, paused, archiving, archived, or deleting; see PROJECT_STATES.
    ("projects", "state", "TEXT NOT NULL DEFAULT 'active'"),
    # The models of a designed factory: JSON {mode, model, thinking}; see checked_factory_models.
    ("projects", "factory_models", "TEXT NOT NULL DEFAULT ''"),
    # Schema version 2.
    # JSON {factory: {roles: {role: {model, thinking}}, states: {state: {model, thinking}}}}.
    ("projects", "factory_model_overrides", "TEXT NOT NULL DEFAULT '{}'"),
    # JSON {factory: true}: factories the user pre-approved for the Captain to start.
    ("projects", "lead_approve_factories", "TEXT NOT NULL DEFAULT '{}'"),
    # JSON per-event wake policy; see DEFAULT_WAKE_POLICY.
    ("projects", "wake_policy", "TEXT NOT NULL DEFAULT '{}'"),
    # The reports.rowid the Captain last read, so a wake sends only newer reports.
    ("projects", "last_report_rowid", "INTEGER NOT NULL DEFAULT 0"),
    # JSON {provider, session, epoch, rules_sha, plan_sha, crew_rules_sha} of the lead's context.
    ("projects", "context_state", "TEXT NOT NULL DEFAULT '{}'"),
    ("projects", "budget_usd", "REAL"),
    ("projects", "budget_tokens", "INTEGER"),
    ("crew", "display_name", "TEXT NOT NULL DEFAULT ''"),
    ("crew", "last_seen_at", "TEXT NOT NULL DEFAULT ''"),
    ("crew", "depends_on", "TEXT NOT NULL DEFAULT ''"),
    ("crew", "pr_url", "TEXT NOT NULL DEFAULT ''"),
    ("crew", "pr_number", "INTEGER"),
    # ISO time before which a queued job does not start.
    ("jobs", "not_before", "TEXT NOT NULL DEFAULT ''"),
    ("jobs", "input_tokens", "INTEGER NOT NULL DEFAULT 0"),
    ("jobs", "output_tokens", "INTEGER NOT NULL DEFAULT 0"),
    ("jobs", "cost_usd", "REAL NOT NULL DEFAULT 0.0"),
    # done, failed, needs_you, interrupted, factory_done, or factory_paused.
    ("reports", "event_kind", "TEXT NOT NULL DEFAULT ''"),
    # The report file under <project>/.prime/captain/reports/.
    ("reports", "report_path", "TEXT NOT NULL DEFAULT ''"),
)
# Schema version 3: one Captain that is not archived per root; archived Captains keep theirs.
SCHEMA_VERSION = 3
ROOT_UNIQUE = re.compile(r"\broot\s+TEXT\s+NOT\s+NULL\s+UNIQUE\b", re.IGNORECASE)


def migrate_schema(db):
    """Bring the store to SCHEMA_VERSION inside the caller's transaction, which runs with
    foreign keys off. Columns are checked against PRAGMA table_info and the projects table
    against its stored SQL, so a store whose version was not recorded upgrades too."""
    if db.execute("PRAGMA user_version").fetchone()[0] >= SCHEMA_VERSION:
        return
    columns = {}
    for table, column, ddl in SCHEMA_COLUMNS:
        if table not in columns:
            columns[table] = {r["name"] for r in db.execute(f"PRAGMA table_info({table})")}
        if column not in columns[table]:
            db.execute(f"ALTER TABLE {table} ADD COLUMN {column} {ddl}")
            columns[table].add(column)
    drop_project_root_unique(db)
    db.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS projects_active_root ON projects(root) WHERE state != 'archived'"
    )
    db.execute(f"PRAGMA user_version={SCHEMA_VERSION}")


def drop_project_root_unique(db):
    """Rebuild a projects table whose root column is UNIQUE without that constraint. The new
    table is the stored CREATE statement with only the constraint removed, so every column,
    default, and the lead constraint stay; rows keep their rowids."""
    sql = db.execute(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='projects'"
    ).fetchone()[0]
    if not ROOT_UNIQUE.search(sql):
        return
    created = re.sub(
        r'^\s*CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?["`\[]?projects["`\]]?',
        "CREATE TABLE projects_new",
        ROOT_UNIQUE.sub("root TEXT NOT NULL", sql, count=1),
        count=1,
        flags=re.IGNORECASE,
    )
    if not created.startswith("CREATE TABLE projects_new"):
        raise sqlite3.DatabaseError("Captain: unexpected projects table definition.")
    names = ",".join(
        '"' + r["name"] + '"' for r in db.execute("PRAGMA table_info(projects)")
    )
    before = db.execute("SELECT count(*) FROM projects").fetchone()[0]
    db.execute("DROP TABLE IF EXISTS projects_new")
    db.execute(created)
    db.execute(
        f"INSERT INTO projects_new(rowid,{names}) SELECT rowid,{names} FROM projects"
    )
    db.execute("DROP TABLE projects")
    db.execute("ALTER TABLE projects_new RENAME TO projects")
    after = db.execute("SELECT count(*) FROM projects").fetchone()[0]
    broken = [r for r in db.execute("PRAGMA foreign_key_check") if r[2] == "projects"]
    if after != before or broken:
        raise sqlite3.IntegrityError("Captain: the projects table rebuild lost rows or references.")


# Tables whose rows change what a project's status file shows, with the project of a row.
TOUCHED_BY = {
    "projects": "{row}.id",
    "crew": "{row}.project",
    "jobs": "{row}.project",
    "reports": "{row}.project",
    "factory_approvals": "{row}.project",
    "pending_grants": "{row}.project",
    "decisions": "{row}.project",
    "schedules": "{row}.project",
    "review_findings": "{row}.project",
    "machines": "(SELECT project FROM main.crew WHERE id={row}.member)",
}


def track_touched_projects(db):
    """Record the project of every row this write transaction changes in temp.touched."""
    db.execute("CREATE TEMP TABLE IF NOT EXISTS touched(project TEXT PRIMARY KEY)")
    for table, project in TOUCHED_BY.items():
        for event, rows in (("INSERT", ("NEW",)), ("UPDATE", ("OLD", "NEW")), ("DELETE", ("OLD",))):
            values = ",".join("(" + project.format(row=row) + ")" for row in rows)
            db.execute(
                f"CREATE TEMP TRIGGER IF NOT EXISTS touched_{table}_{event.lower()} "
                f"AFTER {event} ON main.{table} BEGIN "
                f"INSERT OR IGNORE INTO touched(project) VALUES {values}; END"
            )


def record_edit(db, project, field, old, new, actor, old_value=None):
    """One row of the edit history. `old_value` keeps the previous text for edit-restore."""
    db.execute(
        "INSERT INTO edits(project,field,old_len,new_len,actor,stamp,old_value) VALUES(?,?,?,?,?,?,?)",
        (project, field, old if isinstance(old, int) else len(old or ""),
         new if isinstance(new, int) else len(new or ""), actor, time.time(), old_value),
    )


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
    """The environment of a `pa thread` call. With no native socket named, a turn uses pa's
    default daemon socket, the one a pa chat started without the launcher uses."""
    inherited = os.getenv("PRIME_AGENT_DAEMON_SOCKET")
    native_socket = os.getenv("PA_CAPTAIN_NATIVE_SOCKET") or (
        inherited if inherited and Path(inherited).parent != HUB else None
    )
    env = {
        **{
            k: v
            for k, v in os.environ.items()
            if k not in ("PA_THREAD_MODEL", "PA_THREAD_THINKING", "PA_CAPTAIN_NATIVE_SOCKET")
        },
        "PRIME_AGENT_CODING_AGENT_DIR": str(HUB),
        "PRIME_AGENT_DAEMON_SOCKET": str(HUB / "daemon.sock"),
        "PA_CAPTAIN_HOME": str(HUB),
        "PA_CAPTAIN_NATIVE_HOME": str(NATIVE),
        **extra,
    }
    if native_socket:
        env["PA_CAPTAIN_NATIVE_SOCKET"] = native_socket
    return env


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
            "INSERT OR IGNORE INTO watches(project,session,file,offset) VALUES(?,?,?,?)",
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
    here = current_root()
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
                    "SELECT c.*,m.state AS run_state,m.job AS run_job,m.session AS run_session,m.run_id,m.children AS run_children "
                    "FROM crew c LEFT JOIN machines m ON m.member=c.id WHERE c.project=? ORDER BY c.updated DESC,c.id",
                    (project["id"],),
                )
            ]
            project["pending_grants"] = [
                dict(r, params=json.loads(r["params"] or "{}"))
                for r in db.execute(
                    "SELECT * FROM pending_grants WHERE project=? ORDER BY created_at,id", (project["id"],)
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
                    member.update(run_address(member))
                for key in ("run_session", "run_children"):
                    member.pop(key, None)
                member["session"] = member_session(member)
                member["busy"] = member["id"] in busy
                member["usage"] = member_usage(db, member)
                member["open_findings_count"] = db.execute(
                    "SELECT count(*) FROM review_findings WHERE project=? AND member=? AND status='open'",
                    (project["id"], member["id"]),
                ).fetchone()[0]
                member["pr_status"] = pr_status(member)
                enrich_member(member, project)
            stack_positions(project["crew"])
            project.update(project_notifications(db, project["id"]))
            usage_detail = project_usage_detail(db, project, crew_rows=project["crew"])
            project["usage"] = usage_detail["usage"]
            project["session_costs"] = usage_detail["session_costs"]
            project["budget"] = budget_state(db, project, usage=usage_detail["usage"])
            project["wake_policy"] = wake_policy(project)
            project["schedules"] = project_schedules(db, project["id"])
            project["decisions"] = open_decisions(db, project["id"])
            project["drive_files"] = drive_files(project)
            visible = [m for m in project["crew"] if not m["archived"]]
            project["unread_count"] = sum(m["unread_count"] for m in visible)
            project["unread"] = any(m["unread"] for m in visible)
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
        project["specialists"] = project_specialists(project)
        try:
            project["factories"] = project_factories(project)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            # The panel shows the reason; the rest of the snapshot stays usable.
            project["factories"] = []
            project["factories_error"] = str(error)[:400]
    with database() as db:
        publish_project_statuses(
            db,
            # A project whose factories did not read keeps the factories it last published.
            factories_by_project={
                p["id"]: p["factories"] for p in projects if "factories_error" not in p
            },
        )
        for project in projects:
            status = _status_payload(
                db, project, factories=None if "factories_error" in project else project["factories"]
            )
            project["needs_you"] = status["needs_you"]
            ids = {item.get("member") for item in project["needs_you"]}
            for member in project["crew"]:
                member["needs_you"] = member["id"] in ids
    if not (BASE / "roots.json").is_file():
        write_roots()
    if watching:
        start_watch()
    return {
        "current_root": here,
        "current": next(
            (p["id"] for p in projects if p["root"] == here and p["state"] != "archived"), None
        ),
        "projects": projects,
    }


def activity_state(member):
    """working, waiting, failed, or idle: what the thread is doing, apart from whether it
    needs the user."""
    run = member.get("run_state") or ""
    state = member.get("state") or ""
    if state == "failed" or run in ("failed", "lost"):
        return "failed"
    if run == "paused" or state in ("needs_you", "waiting"):
        return "waiting"
    if member.get("busy") or state == "running" or run in ("queued", "starting", "running", "stopping"):
        return "working"
    return "idle"


_diff_stats = {}


def worktree_diff(cwd, base):
    """(added, deleted) lines of a worktree against its base commit, or None when the folder is
    missing or git is slow."""
    if not cwd or not Path(cwd).is_dir():
        return None
    key = (cwd, base)
    if key not in _diff_stats:
        try:
            run = subprocess.run(
                ["git", "-C", cwd, "diff", "--numstat", base or "HEAD"],
                env=_git_env, capture_output=True, text=True, timeout=2,
            )
        except (OSError, subprocess.SubprocessError):
            return None
        if run.returncode:
            return None
        added = deleted = 0
        for line in run.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) >= 2 and parts[0].isdigit() and parts[1].isdigit():
                added += int(parts[0])
                deleted += int(parts[1])
        _diff_stats[key] = (added, deleted)
    return _diff_stats[key]


def crew_presence(db, member):
    """The fields every crew row shows: its display name, its reports since the user last
    looked, whether it changed since then, and whether a message can steer it mid-turn."""
    seen = parse_iso(member.get("last_seen_at") or "")
    count = db.execute(
        "SELECT count(*) FROM reports WHERE member=? AND stamp>?", (member["id"], seen or 0)
    ).fetchone()[0]
    changed = bool(member.get("updated")) and (seen is None or float(member["updated"]) > seen)
    return {
        "display_name": member_display_name(member),
        "unread": changed or count > 0,
        "unread_count": count,
        "steerable": member.get("provider") == "prime" and member.get("kind") == "native",
    }


def enrich_member(member, project):
    with database() as db:
        member.update(crew_presence(db, member))
    member["updated_at"] = member.get("updated")
    member["activity_state"] = activity_state(member)
    member["archived"] = bool(member.get("archived"))
    member["worktree_missing"] = bool(member.get("worktree_missing"))
    member.setdefault("needs_you", False)
    if member.get("cwd") and member["cwd"] != project["root"] and not member["worktree_missing"]:
        with database() as db:
            row = db.execute(
                "SELECT base_ref FROM jobs WHERE member=? AND base_ref IS NOT NULL ORDER BY created DESC LIMIT 1",
                (member["id"],),
            ).fetchone()
        stats = worktree_diff(member["cwd"], row[0] if row else None) if row else None
        if stats:
            member["diff_added"], member["diff_deleted"] = stats


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
                "SELECT id FROM projects WHERE root=? AND state!='archived'", (current_root(),)
            ).fetchone()
        if not saved:
            return handoff("factory")
        project = saved[0]
    with database() as db:
        if project is None:
            # The Captain that is not archived, else the newest archived one, so
            # `unarchive` works from the project folder.
            found = db.execute(
                "SELECT id FROM projects WHERE root=? ORDER BY state='archived',rowid DESC LIMIT 1",
                (current_root(),),
            ).fetchone()
            if not found:
                raise ValueError("Initialize a Captain for this project first.")
            project = found[0]
        p = record(db, "projects", project)
    member = args.get("member")
    if op == "open" and isinstance(member, str) and member.startswith("step:"):
        return open_step(p, member[5:])
    if op in INBOX_OPS:
        return inbox_action(op, p, args)
    if op in WAVE2_OPS:
        return wave2_action(op, p, args)
    if op == "budget":
        return budget_action(p, args)
    if op == "review-findings":
        return review_findings_action(p, args)
    if op == "review-triage":
        return review_triage(p, args)
    if op == "steer":
        return steer_action(p, args)
    m = None
    if member and member != "lead":
        with database() as db:
            m = record(db, "crew", member)
        if m["project"] != project:
            raise ValueError("Select a member of this project.")
        if op in ("open", "review", "reply", "recover"):
            cwd = durable(member)["cwd"] if m["kind"] == "thread" else m["cwd"]
            if not Path(cwd).is_dir():
                mark_worktree_missing(member, True)
                # A chat still opens on its session; work in the checkout waits for a new one.
                if op != "open" or m["kind"] == "thread" and not thread_session(member):
                    raise ValueError(
                        f"The worktree {cwd} is missing. Recreate it with worktree-recreate "
                        "(Needs you in the Captain panel) before continuing."
                    )
        if m["kind"] == "thread" and op in ("open", "review", "reply", "recover") and Path(cwd).is_dir():
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
            thinking_override = (
                checked_thinking(args.get("thinking")) if "thinking" in args else None
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
                if thinking_override is not None:
                    db.execute(
                        "UPDATE projects SET provider=?,model=?,thinking=? WHERE id=?",
                        (provider, value, thinking_override, project),
                    )
                else:
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
            record_edit(db, project, field, old, value, actor, old)
        if field in DRIVE_FILES:
            sync_drive(dict(p, **{field: value}))
        return {"ok": True, "field": field, "by": actor}
    elif op == "edit-history":
        if actor_kind() == "crew":
            raise ValueError("A crew thread cannot read the edit history.")
        field = args.get("field")
        with database() as db:
            rows = db.execute(
                "SELECT rowid AS id,field,old_len,new_len,actor,stamp,old_value IS NOT NULL AS restorable FROM edits "
                "WHERE project=? AND (? IS NULL OR field=?) ORDER BY stamp DESC,rowid DESC LIMIT 100",
                (project, field, field),
            ).fetchall()
        return {"edits": [dict(r, restorable=bool(r["restorable"])) for r in rows]}
    elif op == "edit-restore":
        if actor_kind() != "user":
            raise ValueError("Only you restore an earlier version.")
        with database() as db:
            row = db.execute(
                "SELECT field,old_value FROM edits WHERE project=? AND rowid=?",
                (project, int(args.get("edit_id"))),
            ).fetchone()
        if not row or row["old_value"] is None or row["field"] not in DRIVE_FILES + ("objective", "name"):
            raise ValueError("That edit has no text to restore.")
        result = action("edit", project=project, field=row["field"], value=row["old_value"])
        return dict(result, restored=int(args["edit_id"]))
    elif op in ("stop", "resume", "delete", "demote", "archive-project", "unarchive-project"):
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
                "INSERT OR IGNORE INTO crew(id,project,kind,ref,title,cwd,provider,state,detail,updated,model,thinking) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
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
                    "",
                    "",
                ),
            )
            if kind == "native":
                watch_native(db, project, ref, True)
            else:
                file = HUB / "threads" / (mid + ".json")
                db.execute(
                    "INSERT OR IGNORE INTO watches(project,session,file,offset) VALUES(?,?,?,?)",
                    (project, ref, str(file), len(t["messages"])),
                )
        start_watch()
        return {"id": mid}
    elif op == "new-thread":
        if actor_kind() != "user":
            raise ValueError("Only you open a new crew thread.")
        model = thinking = ""
        if args.get("model"):
            _, model = checked_model("prime", args["model"])
        if args.get("thinking"):
            thinking = checked_thinking(args["thinking"])
        mid = str(uuid.uuid4())
        cwd = p["root"]
        if args.get("write"):
            base = git_at(p["root"], "rev-parse", "--verify", "HEAD^{commit}")
            if base.returncode:
                raise ValueError("An isolated writer needs a valid base commit.")
            cwd = str(BASE / "worktrees" / mid)
            add_worktree(p, {"id": mid, "cwd": cwd}, {"base_ref": base.stdout.decode().strip()})
        title = checked_text(str(args.get("title") or "New crew chat"), 180)
        display_name = make_display_name("native", title, "", mid)
        session = native_session(mid, cwd, display_name)
        with database(True) as db:
            db.execute(
                "INSERT INTO crew(id,project,kind,ref,title,cwd,provider,state,detail,updated,model,thinking,display_name) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (mid, project, "native", str(session), title, cwd, "prime", "ready", "", time.time(), model, thinking, display_name),
            )
            watch_native(db, project, session, True)
        start_watch()
        result = {"ok": True, "member": mid, "session": str(session), "cwd": cwd}
        if os.getenv("PA_CAPTAIN_SELECTION"):
            result.update(handoff("resume", str(session)))
        return result
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
    elif op == "crew-model" and m:
        actor = actor_kind()
        if actor == "crew":
            raise ValueError("A crew thread cannot change models.")
        if m["kind"] == "factory":
            raise ValueError("Select a crew thread to change its model.")
        provider = args.get("provider") or m["provider"]
        _, model = checked_model(provider, args.get("model"))
        thinking = (
            checked_thinking(args.get("thinking"))
            if "thinking" in args
            else (m.get("thinking") or "")
        )
        with database(True) as db:
            db.execute(
                "UPDATE crew SET provider=?,model=?,thinking=?,updated=? WHERE id=? AND project=?",
                (provider, model, thinking, time.time(), member, project),
            )
            record_edit(db, project, "crew_model " + member[:8], m.get("model") or "", model, actor)
        return {"ok": True, "id": member, "provider": provider, "model": model, "thinking": thinking}
    elif op in ("lead", "reply", "delegate"):
        require_active(p)
        if op == "delegate" and not args.get("request_id") and actor_kind() == "user":
            # A retried click of the same assignment within the window dedupes onto one job.
            args = dict(args, request_id=default_request_id(op, project, member, args))
        return dispatch_action(p, op, m, args)
    elif op == "grant":
        return grant_machine(p, args)
    elif op in ("grant-approve", "grant-decline"):
        return resolve_pending_grant(op, p, args)
    elif op == "worktree-recreate":
        if actor_kind() != "user":
            raise ValueError("Only you recreate a worktree.")
        if not m:
            raise ValueError("Choose the crew thread whose worktree is missing.")
        return recreate_worktree(p, m)
    elif op == "checkpoint-rollback":
        if actor_kind() != "user":
            raise ValueError("Only you roll back a worktree.")
        if not m:
            raise ValueError("Choose the crew thread to roll back.")
        return checkpoint_rollback(p, m, args)
    elif op in ("archive", "unarchive") and not member:
        return project_state_action(op + "-project", p)
    elif op in ("archive", "unarchive"):
        if actor_kind() == "crew":
            raise ValueError("A crew thread cannot archive threads.")
        if not m:
            raise ValueError("Choose a crew thread to archive.")
        with database(True) as db:
            if op == "archive" and db.execute(
                "SELECT 1 FROM jobs WHERE member=? AND status IN ('queued','running')", (member,)
            ).fetchone():
                raise ValueError("Wait for the active assignment before archiving its thread.")
        # A job marked failed can leave its daemon turn running, unseen once archived.
        session = member_session(m) if op == "archive" else None
        if session and daemon_turn_busy(session):
            if args.get("force") is not True:
                raise ValueError("This thread still has an active daemon turn; stop it before archiving.")
            daemon_abort(session)
        with database(True) as db:
            db.execute(
                "UPDATE crew SET archived=? WHERE id=? AND project=?",
                (1 if op == "archive" else 0, member, project),
            )
        return {"ok": True, "id": member, "archived": op == "archive"}
    elif op == "consensus":
        require_active(p)
        task = checked_text(args["task"])
        # The user asked for it from the panel: start it now, with no chat draft to send.
        return dispatch_action(p, "delegate", None, {
            "machine": "dual-consensus", "task": task, "approved": True,
            "request_id": args.get("request_id") or default_request_id("consensus", project, task),
        })
    elif op in MODULE_OPS or op in SPECIALIST_OPS:
        return module_action(op, p, args)
    elif op.startswith("factory-"):
        return factory_action(op, p, args)
    elif op == "answer":
        if not m or m["kind"] != "factory":
            raise ValueError("Choose a factory run to answer.")
        return answer_run(p, m, args)
    elif op == "factory":
        require_active(p)
        local = current_root() == p["root"]
        session = Path(SESSION) if local and SESSION else None
        if not session or not session.is_file():
            session = factory_session(p)
            local = False
        with database(True) as db:
            watch_native(db, project, session, False)
        start_watch()
        if args.get("machine"):
            draft = factory_draft(args["machine"], p["objective"])
            return (
                handoff("draft", draft)
                if local
                else handoff("resume", str(session), draft=draft)
            )
        return (
            handoff("factory")
            if local
            else handoff("resume", str(session), panel="factory")
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
            with database(True) as db:
                db.execute("UPDATE crew SET last_seen_at=? WHERE id=?", (iso_now(), m["id"]))
        else:
            ensure_lead(p)
            session = thread_session(p["lead"])
            title = "captain"
        if session:
            live_factory = bool(
                m
                and m["kind"] == "factory"
                and m.get("state") in ("running", "needs_you")
            )
            return handoff(
                "resume",
                session,
                **({"panel": "factory"} if live_factory else {}),
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


INBOX_OPS = ("wake-policy", "notifications", "notifications-read", "mark-read")


def open_step(p, ref):
    """The native session of one factory step: a session file path, or a subagent child ID the
    daemon roster knows."""
    path = Path(ref).expanduser()
    if ref.endswith(".jsonl") and path.is_absolute() and path.is_file():
        return {"ok": True, "kind": "native", "session": str(path.resolve()), "cwd": p["root"]}
    if not re.fullmatch(r"sub-[A-Za-z0-9_-]{1,64}", ref):
        raise ValueError("Open a step by its session file or its sub- child ID.")
    try:
        listed = daemon_request({"type": "list", "includeClientOwned": True})
    except (OSError, ValueError) as error:
        raise ValueError(f"The daemon cannot list step sessions: {error}") from None
    for row in (listed.get("data") or {}).get("sessions") or []:
        meta = row.get("runtimeMetadata") or row.get("runtime_metadata") or {}
        if ref in (row.get("rlmChildId"), meta.get("rlmChildId")) and row.get("sessionFile"):
            return {"ok": True, "kind": "native", "session": row["sessionFile"],
                    "session_id": row.get("sessionId") or row.get("id"), "cwd": p["root"]}
    raise ValueError(f"No live step session has the child ID {ref}.")


def inbox_action(op, p, args):
    """The wake policy, the notification inbox, and crew unread markers."""
    actor = actor_kind()
    if actor == "crew" or (op != "wake-policy" and actor != "user"):
        raise ValueError("Only you manage the Captain's notifications.")
    if op == "wake-policy":
        policy = wake_policy(p)
        if args.get("policy") is None:
            return {"ok": True, "policy": policy}
        if actor == "captain" and captain_turn(p) != "lead":
            raise ValueError("Change the wake policy only in a turn that answers a user message.")
        policy = checked_wake_policy(policy, args["policy"])
        with database(True) as db:
            db.execute("UPDATE projects SET wake_policy=? WHERE id=?", (json.dumps(policy), p["id"]))
            record_edit(db, p["id"], "wake_policy", p.get("wake_policy") or "{}", json.dumps(policy), actor)
        return {"ok": True, "policy": policy, "by": actor}
    with database(True) as db:
        if op == "notifications":
            return dict(project_notifications(db, p["id"], int(args.get("limit") or 50)), ok=True)
        if op == "notifications-read":
            if args.get("id") is not None:
                count = db.execute(
                    "UPDATE notifications SET read_at=? WHERE id=? AND project=? AND read_at=''",
                    (iso_now(), int(args["id"]), p["id"]),
                ).rowcount
            else:
                count = db.execute(
                    "UPDATE notifications SET read_at=? WHERE project=? AND read_at=''", (iso_now(), p["id"])
                ).rowcount
            return {"ok": True, "read": count}
        member = args.get("member")
        if member:
            count = db.execute(
                "UPDATE crew SET last_seen_at=? WHERE id=? AND project=?", (iso_now(), member, p["id"])
            ).rowcount
            if not count:
                raise ValueError("Select a member of this project.")
        else:
            count = db.execute("UPDATE crew SET last_seen_at=? WHERE project=?", (iso_now(), p["id"])).rowcount
        return {"ok": True, "marked": count}


def project_notifications(db, project, limit=20):
    rows = db.execute(
        "SELECT * FROM notifications WHERE project=? ORDER BY id DESC LIMIT ?", (project, max(1, min(limit, 200)))
    ).fetchall()
    unread = db.execute(
        "SELECT count(*) FROM notifications WHERE project=? AND read_at=''", (project,)
    ).fetchone()[0]
    return {"notifications": [dict(r, unread=not r["read_at"]) for r in rows], "unread_notifications": unread}


def stack_positions(crew):
    """Set stack_position "n/total" on each member of a depends_on chain, None elsewhere."""
    parents = {m["id"]: m.get("depends_on") or "" for m in crew}
    children = {}
    for mid, parent in parents.items():
        if parent:
            children.setdefault(parent, []).append(mid)

    def depth_below(mid, seen=()):
        return max((1 + depth_below(c, seen + (mid,)) for c in children.get(mid, ()) if c not in seen), default=0)

    for m in crew:
        above, cursor, seen = 0, m["id"], {m["id"]}
        while parents.get(cursor) and parents[cursor] not in seen:
            cursor = parents[cursor]
            seen.add(cursor)
            above += 1
        below = depth_below(m["id"])
        m["depends_on"] = m.get("depends_on") or ""
        m["stack_position"] = f"{above + 1}/{above + 1 + below}" if above or below else None


def drive_files(p):
    """Report and handoff files under <project>/.prime/captain/, as paths relative to it."""
    folder = Path(p["root"]) / ".prime" / "captain"
    found = []
    for pattern in ("reports/*.md", "handoffs/*.json"):
        found += sorted(folder.glob(pattern), key=lambda f: f.stat().st_mtime, reverse=True)
    return [str(f.relative_to(folder)) for f in found[:200]]


def budget_state(db, p, usage=None):
    if usage is None:
        usage = project_usage_detail(db, p)["usage"]
    spent_tokens = usage["input_tokens"] + usage["output_tokens"]
    cap_usd, cap_tokens = p.get("budget_usd"), p.get("budget_tokens")
    exceeded = (cap_usd is not None and usage["cost_usd"] >= cap_usd) or (
        cap_tokens is not None and spent_tokens >= cap_tokens
    )
    return {"budget_usd": cap_usd, "budget_tokens": cap_tokens, "spent_usd": usage["cost_usd"],
            "spent_tokens": spent_tokens, "exceeded": bool(exceeded)}


def checked_amount(value, kind):
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value < 0:
        raise ValueError(f"Pass {kind} as a number of zero or more.")
    return int(value) if kind.endswith("tokens") else round(float(value), 6)


def budget_action(p, args):
    """Read the project's spend cap; only the user sets, extends, or clears it. A higher cap
    lets held digest turns run again."""
    keys = ("budget_usd", "budget_tokens", "extend_usd", "extend_tokens", "clear")
    actor = actor_kind()
    if actor == "crew":
        raise ValueError("A crew thread cannot read the budget.")
    if any(k in args for k in keys):
        if actor != "user":
            raise ValueError("Only you set the Captain's budget.")
        with database(True) as db:
            p = record(db, "projects", p["id"])
            spent = budget_state(db, p)
            usd, tokens = p["budget_usd"], p["budget_tokens"]
            if args.get("clear") is True:
                usd = tokens = None
            if "budget_usd" in args:
                usd = checked_amount(args["budget_usd"], "budget_usd")
            if "budget_tokens" in args:
                tokens = checked_amount(args["budget_tokens"], "budget_tokens")
            if args.get("extend_usd") is not None:
                usd = round((usd if usd is not None else spent["spent_usd"]) + checked_amount(args["extend_usd"], "extend_usd"), 6)
            if args.get("extend_tokens") is not None:
                tokens = (tokens if tokens is not None else spent["spent_tokens"]) + checked_amount(args["extend_tokens"], "extend_tokens")
            db.execute("UPDATE projects SET budget_usd=?,budget_tokens=? WHERE id=?", (usd, tokens, p["id"]))
            record_edit(db, p["id"], "budget", json.dumps([p["budget_usd"], p["budget_tokens"]]), json.dumps([usd, tokens]), actor)
        kick_queue()
    with database() as db:
        return {"ok": True, "budget": budget_state(db, record(db, "projects", p["id"]))}


FINDING_STATUSES = ("open", "resolved", "irrelevant")
MAX_FINDINGS = 500


def snippet_digest(snippet):
    """SHA-256 of a snippet with its whitespace normalized, so a moved or reindented line keeps its key."""
    lines = (" ".join(line.split()) for line in str(snippet).splitlines())
    return hashlib.sha256("\n".join(line for line in lines if line).encode()).hexdigest()


def review_findings_action(p, args):
    """Upsert review findings keyed by (member, file, snippet hash, category), keeping their
    triage status, then list the findings of the project."""
    if actor_kind() == "crew":
        raise ValueError("A crew thread cannot record review findings.")
    member = str(args.get("member") or "")
    status = args.get("status")
    if status is not None and status not in FINDING_STATUSES:
        raise ValueError("Filter status by open, resolved, or irrelevant.")
    findings = args.get("findings")
    upserted = 0
    if findings is not None:
        if not isinstance(findings, list) or len(findings) > MAX_FINDINGS:
            raise ValueError(f"Pass findings as a list of at most {MAX_FINDINGS} objects.")
        now = iso_now()
        with database(True) as db:
            for f in findings:
                if not isinstance(f, dict):
                    raise ValueError("Pass each finding as an object.")
                owner = str(f.get("member") or member)
                if owner and not db.execute("SELECT 1 FROM crew WHERE id=? AND project=?", (owner, p["id"])).fetchone():
                    raise ValueError("Select a member of this project.")
                if not f.get("snippet_hash") and not str(f.get("snippet") or "").strip():
                    raise ValueError("Give each finding a snippet or a snippet_hash.")
                key = (
                    p["id"], owner, checked_text(str(f.get("file") or ""), 1000),
                    checked_text(str(f["snippet_hash"]), 128) if f.get("snippet_hash") else snippet_digest(f["snippet"]),
                    checked_text(str(f.get("category") or ""), 100),
                )
                line = f.get("line_hint")
                values = (
                    checked_text(str(f.get("severity") or ""), 40),
                    str(f.get("confidence") or "Confirmed")[:40],
                    int(line) if isinstance(line, int) and not isinstance(line, bool) else None,
                    checked_text(str(f.get("message") or ""), 4000),
                    now,
                )
                row = db.execute(
                    "SELECT id FROM review_findings WHERE project=? AND member=? AND file=? AND snippet_hash=? AND category=?",
                    key,
                ).fetchone()
                if row:
                    db.execute(
                        "UPDATE review_findings SET severity=?,confidence=?,line_hint=?,message=?,updated_at=? WHERE id=?",
                        (*values, row[0]),
                    )
                else:
                    db.execute(
                        "INSERT INTO review_findings(id,project,member,file,snippet_hash,category,severity,confidence,"
                        "line_hint,message,status,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,'open',?,?)",
                        (str(uuid.uuid4()), *key, *values, now),
                    )
                upserted += 1
    with database() as db:
        rows = db.execute(
            "SELECT * FROM review_findings WHERE project=? AND (?='' OR member=?) AND (? IS NULL OR status=?) "
            "ORDER BY created_at,id",
            (p["id"], member, member, status, status),
        ).fetchall()
    return {"ok": True, "findings": [dict(r) for r in rows], "upserted": upserted}


def review_triage(p, args):
    if actor_kind() != "user":
        raise ValueError("Only you triage review findings.")
    status = args.get("status")
    if status not in FINDING_STATUSES:
        raise ValueError("Set status to open, resolved, or irrelevant.")
    with database(True) as db:
        count = db.execute(
            "UPDATE review_findings SET status=?,updated_at=? WHERE id=? AND project=?",
            (status, iso_now(), str(args.get("id") or ""), p["id"]),
        ).rowcount
    if not count:
        raise ValueError("That finding does not exist in this project.")
    return {"ok": True, "id": args["id"], "status": status}


def job_usage(db, where, value):
    row = db.execute(
        f"SELECT coalesce(sum(input_tokens),0),coalesce(sum(output_tokens),0),coalesce(sum(cost_usd),0) FROM jobs WHERE {where}=?",
        (value,),
    ).fetchone()
    return {"input_tokens": row[0], "output_tokens": row[1], "cost_usd": round(row[2], 6)}


_CLI_COST_APIS = {"antigravity-cli", "claude-cli", "cursor-cli"}
_CLI_COST_PROVIDERS = {"antigravity", "cursor", "claude-cli"}
_USAGE_FILE_CACHE = {}
_USAGE_CACHE_LOADED = False
_USAGE_CACHE_DIRTY = False


def _usage_cache_path():
    return BASE / "usage-cache.json"


def _load_usage_cache():
    global _USAGE_CACHE_LOADED
    if _USAGE_CACHE_LOADED:
        return
    _USAGE_CACHE_LOADED = True
    try:
        raw = json.loads(_usage_cache_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return
    if not isinstance(raw, dict):
        return
    for path_str, entry in raw.items():
        if isinstance(entry, list) and len(entry) == 6:
            _USAGE_FILE_CACHE[path_str] = tuple(entry)


def _save_usage_cache():
    global _USAGE_CACHE_DIRTY
    if not _USAGE_CACHE_DIRTY:
        return
    _USAGE_CACHE_DIRTY = False
    try:
        if len(_USAGE_FILE_CACHE) > 4096:
            # Keep the most recent entries by mtime_ns.
            trimmed = dict(
                sorted(_USAGE_FILE_CACHE.items(), key=lambda kv: kv[1][0], reverse=True)[:4096]
            )
            _USAGE_FILE_CACHE.clear()
            _USAGE_FILE_CACHE.update(trimmed)
        atomic_write_json(
            _usage_cache_path(),
            {k: list(v) for k, v in _USAGE_FILE_CACHE.items()},
        )
    except OSError:
        pass


def _cli_list_price(model):
    """Per-million USD list prices (input, output, cache_read, cache_write) matching pa-ai."""
    low = str(model or "").strip().lower()
    if "/" in low:
        low = low.rsplit("/", 1)[-1]
    if not low or low == "auto":
        return None
    if "gemini-4-neon" in low or "gemini-3.5-pro" in low or "gemini-2.5-pro" in low:
        return (2.5, 15.0, 0.25, 0.0)
    if "gemini-4-argon" in low or "gemini-pro-orchestrator" in low:
        return (1.25, 10.0, 0.125, 0.0)
    if "gemini-4-hydrogen" in low or "gemini-3.8-flash" in low or "gemini" in low and "flash" in low:
        return (0.5, 3.0, 0.05, 0.0)
    if "gemini" in low:
        return (2.5, 15.0, 0.25, 0.0)
    if "claude-opus-4-1" in low or "claude-opus-4.1" in low or low == "claude-opus-4" or low.startswith("claude-opus-4-2025"):
        return (15.0, 75.0, 1.5, 18.75)
    if "claude-opus" in low:
        return (5.0, 25.0, 0.5, 6.25)
    if "claude-sonnet" in low:
        return (3.0, 15.0, 0.3, 3.75)
    if "claude-haiku" in low:
        return (1.0, 5.0, 0.1, 1.25)
    if "composer" in low:
        return (1.25, 6.0, 0.125, 0.0)
    if "gpt-5" in low or "gpt-6" in low or "codex" in low:
        return (2.5, 15.0, 0.25, 0.0)
    return None


def _session_file_usage(path):
    """Return (session_id, input_tokens, output_tokens, cost_usd) for one JSONL file."""
    global _USAGE_CACHE_DIRTY
    _load_usage_cache()
    try:
        resolved = Path(path).expanduser().resolve(strict=True)
        stat = resolved.stat()
    except OSError:
        return None, 0, 0, 0.0
    path_str = str(resolved)
    cached = _USAGE_FILE_CACHE.get(path_str)
    if cached is not None and cached[0] == stat.st_mtime_ns and cached[1] == stat.st_size:
        return cached[2], cached[3], cached[4], cached[5]
    sid = None
    inp = 0
    out = 0
    cost = 0.0
    try:
        with resolved.open("r", encoding="utf-8", errors="replace") as stream:
            for idx, line in enumerate(stream):
                if not line.strip():
                    continue
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                if idx == 0 and isinstance(row, dict) and row.get("type") == "session":
                    sid = str(row.get("id") or "") or None
                    continue
                if not isinstance(row, dict):
                    continue
                msg = row.get("message") if isinstance(row.get("message"), dict) else row
                if not isinstance(msg, dict) or msg.get("role") != "assistant":
                    continue
                usage = msg.get("usage")
                if not isinstance(usage, dict):
                    continue
                u_in = int(usage.get("input") or 0)
                u_out = int(usage.get("output") or 0)
                u_cr = int(usage.get("cacheRead") or 0)
                u_cw = int(usage.get("cacheWrite") or 0)
                inp += u_in
                out += u_out
                u_cost = usage.get("cost")
                total = float(u_cost.get("total") or 0.0) if isinstance(u_cost, dict) else 0.0
                if total <= 0.0 and (u_in + u_out + u_cr + u_cw) > 0:
                    api = str(msg.get("api") or "")
                    provider = str(msg.get("provider") or "")
                    if api in _CLI_COST_APIS or provider in _CLI_COST_PROVIDERS:
                        rates = _cli_list_price(msg.get("responseModel") or msg.get("model"))
                        if rates is not None:
                            r_in, r_out, r_cr, r_cw = rates
                            total = (
                                u_in * r_in + u_out * r_out + u_cr * r_cr + u_cw * r_cw
                            ) / 1_000_000.0
                cost += max(0.0, total)
    except OSError:
        return None, 0, 0, 0.0
    final_sid = sid or resolved.stem
    _USAGE_FILE_CACHE[path_str] = (stat.st_mtime_ns, stat.st_size, final_sid, inp, out, cost)
    _USAGE_CACHE_DIRTY = True
    return final_sid, inp, out, cost


def _session_tree_usage(session_path, seen=None):
    """Sum usage across a root session JSONL and all recursive subagent JSONLs under session-artifacts."""
    if not session_path:
        return None, {"input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0}
    try:
        root = Path(session_path).expanduser().resolve(strict=True)
    except OSError:
        return None, {"input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0}
    if seen is None:
        seen = set()
    artifacts = NATIVE / "session-artifacts"
    queue = [root]
    root_sid = None
    total_in = 0
    total_out = 0
    total_cost = 0.0
    while queue:
        current = queue.pop(0)
        key = str(current)
        if key in seen:
            continue
        seen.add(key)
        sid, inp, out, cost = _session_file_usage(current)
        if root_sid is None:
            root_sid = sid
        total_in += inp
        total_out += out
        total_cost += cost
        if sid:
            sub_base = artifacts / sid
            if sub_base.is_dir():
                try:
                    for sub_dir in sub_base.iterdir():
                        if not sub_dir.is_dir() or not sub_dir.name.startswith("sub-"):
                            continue
                        for child_file in sub_dir.glob("*.jsonl"):
                            try:
                                resolved_child = child_file.resolve(strict=True)
                            except OSError:
                                continue
                            if str(resolved_child) not in seen:
                                queue.append(resolved_child)
                except OSError:
                    pass
    return root_sid, {
        "input_tokens": total_in,
        "output_tokens": total_out,
        "cost_usd": round(total_cost, 6),
    }


def _merge_usage(a, b):
    return {
        "input_tokens": max(int(a.get("input_tokens") or 0), int(b.get("input_tokens") or 0)),
        "output_tokens": max(int(a.get("output_tokens") or 0), int(b.get("output_tokens") or 0)),
        "cost_usd": round(max(float(a.get("cost_usd") or 0.0), float(b.get("cost_usd") or 0.0)), 6),
    }


def member_usage(db, member):
    jobs = job_usage(db, "member", member["id"])
    _, tree = _session_tree_usage(member_session(member))
    return _merge_usage(jobs, tree)


def project_usage_detail(db, project, crew_rows=None):
    """Project-wide spend across the Captain lead, every crew thread/factory (including archived),
    and all of their subagent trees, merged with SQLite job usage."""
    jobs = job_usage(db, "project", project["id"])
    if crew_rows is None:
        crew_rows = [dict(r) for r in db.execute("SELECT * FROM crew WHERE project=?", (project["id"],))]
    seen = set()
    session_costs = {}
    tree_in = 0
    tree_out = 0
    tree_cost = 0.0

    lead_path = thread_session(project["lead"])
    lead_sid, lead_usage = _session_tree_usage(lead_path, seen=seen)
    if lead_sid:
        session_costs[lead_sid] = lead_usage["cost_usd"]
    tree_in += lead_usage["input_tokens"]
    tree_out += lead_usage["output_tokens"]
    tree_cost += lead_usage["cost_usd"]

    for member in crew_rows:
        m_path = member_session(member)
        m_sid, m_usage = _session_tree_usage(m_path, seen=seen)
        if m_sid:
            session_costs[m_sid] = round(session_costs.get(m_sid, 0.0) + m_usage["cost_usd"], 6)
        tree_in += m_usage["input_tokens"]
        tree_out += m_usage["output_tokens"]
        tree_cost += m_usage["cost_usd"]

    tree_total = {
        "input_tokens": tree_in,
        "output_tokens": tree_out,
        "cost_usd": round(tree_cost, 6),
    }
    _save_usage_cache()
    return {
        "usage": _merge_usage(jobs, tree_total),
        "lead_session_id": lead_sid or "",
        "session_costs": session_costs,
    }


REQUEST_WINDOW = 30


def default_request_id(*parts):
    """A request ID for an action the caller sent without one: the same action within
    REQUEST_WINDOW seconds gets the same ID."""
    body = json.dumps([*parts, int(time.time() // REQUEST_WINDOW)], sort_keys=True, default=str)
    return "auto-" + hashlib.sha256(body.encode()).hexdigest()[:24]


# The project fields that mirror to <project>/.prime/captain/<file>.
DRIVE_FILES = ("rules", "crew_rules", "plan", "digest")
DRIVE_NAMES = {"rules": "AGENTS.md", "crew_rules": "CREW.md", "plan": "NOTES.md", "digest": "DIGEST.md"}


def sync_drive(p):
    """Write the project's Captain files. The database stays the source; a failed write is
    rewritten on the next change."""
    folder = Path(p["root"]) / ".prime" / "captain"
    try:
        for field in DRIVE_FILES:
            path = folder / DRIVE_NAMES[field]
            text = (p.get(field) or "").rstrip() + "\n"
            if not path.is_file() or path.read_text(errors="replace") != text:
                atomic_write_text(path, text)
    except OSError as error:
        print(f"Captain: could not write {folder}: {error}", file=sys.stderr)


def mark_worktree_missing(member, missing):
    with database(True) as db:
        db.execute(
            "UPDATE crew SET worktree_missing=? WHERE id=? AND worktree_missing!=?",
            (int(missing), member, int(missing)),
        )


def check_worktrees():
    """Flag writer threads whose worktree folder is gone, and clear the flag when it is back."""
    with database() as db:
        rows = db.execute(
            "SELECT c.id,c.cwd,c.worktree_missing,p.root FROM crew c JOIN projects p ON p.id=c.project "
            "WHERE c.cwd!=p.root AND c.kind IN ('thread','factory') AND NOT EXISTS "
            "(SELECT 1 FROM jobs j WHERE j.member=c.id AND j.status IN ('queued','running')) "
            "AND EXISTS (SELECT 1 FROM jobs j WHERE j.member=c.id AND j.base_ref IS NOT NULL AND j.status NOT IN ('queued','cancelled'))"
        ).fetchall()
    for row in rows:
        missing = not Path(row["cwd"]).is_dir()
        if missing != bool(row["worktree_missing"]):
            mark_worktree_missing(row["id"], missing)


def recreate_worktree(p, m):
    """Put a missing writer worktree back on its branch pa/<member>, or on its base commit
    when the branch is gone. Never resets or deletes work."""
    cwd = Path(m["cwd"])
    if cwd.is_dir():
        mark_worktree_missing(m["id"], False)
        return {"ok": True, "id": m["id"], "cwd": str(cwd), "repeat": True}
    if str(cwd) == p["root"]:
        raise ValueError("This thread works in the project checkout.")
    git_at(p["root"], "worktree", "prune")
    branch = "pa/" + m["id"]
    cwd.parent.mkdir(parents=True, exist_ok=True)
    if git_at(p["root"], "rev-parse", "--verify", "--quiet", "refs/heads/" + branch).returncode == 0:
        add = git_at(p["root"], "worktree", "add", "--", str(cwd), branch)
    else:
        with database() as db:
            row = db.execute(
                "SELECT base_ref FROM jobs WHERE member=? AND base_ref IS NOT NULL ORDER BY created DESC LIMIT 1",
                (m["id"],),
            ).fetchone()
        if not row:
            raise ValueError("This thread has no branch or base commit to recreate its worktree from.")
        add = git_at(p["root"], "worktree", "add", "-b", branch, "--", str(cwd), row[0])
    if add.returncode:
        raise ValueError(add.stderr.decode(errors="replace").strip()[-800:])
    mark_worktree_missing(m["id"], False)
    return {"ok": True, "id": m["id"], "cwd": str(cwd), "branch": branch}


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
PROVIDERS = ("prime", "claude", "codex", "antigravity", "cursor")
PROVIDER_CHOICE = "Choose Prime, Claude, Codex, Antigravity, or Cursor."
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
    return thread_models("antigravity")


@functools.lru_cache(maxsize=1)
def cursor_models():
    """Model IDs reported by `pa thread models cursor`; none when this pa cannot list them."""
    return thread_models("cursor")


SUMMARY_MODELS_PER_PROVIDER = 3
FEATURED_MODELS = ("claude-opus-5-5", "gpt-5.3-codex", "gemini-4-neon", "gemini-4-argon")


def _ranked_models(models):
    """Featured models first, then the highest version numbers."""
    ranked = sorted(models, key=lambda m: (tuple(int(n) for n in re.findall(r"\d+", m)), m), reverse=True)
    return sorted(ranked, key=lambda m: m not in FEATURED_MODELS)


@functools.lru_cache(maxsize=1)
def configured_models_summary():
    """One line naming the models this user can run, a few per provider; empty when no
    provider is configured. It sits in the cache-stable menus, so it must be deterministic."""
    groups = {}
    for provider, model in model_catalog():
        groups.setdefault(provider, []).append(model)
    for provider, ids in (("antigravity", antigravity_models()), ("cursor", cursor_models())):
        if ids and provider not in groups:
            groups[provider] = [mid.removeprefix(provider + "/") for mid in ids]
    parts = []
    for provider, models in groups.items():
        # Dated snapshots and -latest aliases repeat a base model already listed.
        base = [m for m in models if not m.endswith("-latest") and not m[-8:].isdecimal()] or models
        shown = _ranked_models(set(base))
        line = provider + ": " + ", ".join(shown[:SUMMARY_MODELS_PER_PROVIDER])
        if len(shown) > SUMMARY_MODELS_PER_PROVIDER:
            line += f" (+{len(shown) - SUMMARY_MODELS_PER_PROVIDER} more)"
        parts.append(line)
    if not parts:
        return ""
    return "Configured models (use only these, as provider/model): " + " | ".join(parts) + "\n"


@functools.lru_cache(maxsize=1)
def fsm_guide_path():
    """The Captain's FSM authoring guide: beside the executable (packaged), in the source
    checkout above it, or in the native home's installed skills; empty when none exists."""
    relative = Path("skills/captain/references/fsm-design-guide.md")
    roots = [*Path(PA).parents, NATIVE]
    for root in roots:
        candidate = root / relative
        if candidate.is_file():
            return str(candidate)
    return ""


def thread_models(provider):
    try:
        run = subprocess.run(
            [PA, "thread", "models", provider],
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
    choices.append(("cursor", ""))
    choices += [("cursor", mid) for mid in cursor_models()]
    return choices


def checked_model(provider, model):
    if provider not in PROVIDERS:
        raise ValueError(PROVIDER_CHOICE)
    model = str(model or "").strip()
    if model and provider != "prime" and "/" in model:
        family, rest = model.split("/", 1)
        if provider in PROVIDER_FAMILIES and family in PROVIDER_FAMILIES[provider]:
            model = rest
        elif provider in ("antigravity", "cursor") and family == provider:
            model = rest
    if model and (provider, model) not in model_choices():
        if provider == "antigravity":
            # Also allow explicit effort-suffixed IDs like `gemini-4-neon-high` when base is listed.
            base = model.rsplit("-", 1)[0] if "-" in model else ""
            if base and (provider, base) in model_choices():
                return provider, model
        raise ValueError(f"Unknown {provider} model {model}. See pa model list.")
    return provider, model


# How a designed factory chooses models: one model for every step (the Captain's, or one the
# user picked), or a model per step that the Captain chooses.
FACTORY_MODEL_MODES = ("captain", "model", "choose")


def checked_factory_models(value):
    """The stored factory model choice: "" (none) or JSON {mode, model, thinking}."""
    if value in (None, "", {}):
        return ""
    if not isinstance(value, dict) or value.get("mode") not in FACTORY_MODEL_MODES:
        raise ValueError("Pass factory_models as {mode: captain, model, or choose}.")
    choice = {"mode": value["mode"]}
    if value["mode"] == "model":
        _, model = checked_model("prime", value.get("model"))
        if not model:
            raise ValueError("Choose the model for every factory step.")
        choice["model"] = model
        choice["thinking"] = checked_thinking(value.get("thinking"))
    return json.dumps(choice, sort_keys=True)


def factory_models(p):
    try:
        choice = json.loads(p.get("factory_models") or "{}")
    except ValueError:
        return {}
    return choice if isinstance(choice, dict) else {}


def factory_default_model(p):
    """The model and thinking of a factory step that names no model or role: the model the user
    picked for the factory, or else the Captain's own model as a pa model ID. An empty model is
    pa's default model."""
    choice = factory_models(p)
    if choice.get("mode") == "model":
        return choice.get("model") or "", choice.get("thinking") or ""
    model = p.get("model") or ""
    if model and p.get("provider", "prime") != "prime" and "/" not in model:
        model = next((a + "/" + b for a, b in model_catalog() if b == model), "")
    return model, p.get("thinking") or ""


def factory_model_rule(p):
    """The design turn's instruction for factory models, from the project's choice."""
    choice = factory_models(p)
    if choice.get("mode") in ("captain", "model"):
        model, thinking = factory_default_model(p)
        if not model:
            return "Models: give agent steps no model or role, so they run on the factory default. "
        level = f', thinking = "{thinking}"' if thinking else ""
        return (
            f'Models: write one role in machine.toml, [roles] default = {{ model = "{model}"{level} }}, '
            'and use agent = { role = "default" } in every agent step that has no specialist. '
        )
    if choice.get("mode") == "choose":
        return (
            "Models: choose a model for each agent step from pa model list (provider/model). Put each "
            "choice in [roles] and name it with agent = { role = ... }; add a default role for steps "
            "with no special need. "
        )
    return ""


STATUS_DIR = BASE / "status"


def _member_label(member):
    if member["kind"] == "factory":
        return member.get("machine") or member["title"].split(" · ", 1)[0]
    return " ".join(str(member.get("title") or "").split())


def _member_state_text(member):
    run = member.get("run_state") or ""
    state = member.get("state") or ""
    if state == "needs_you":
        return "paused" if run == "paused" else "needs you"
    if state == "running":
        return "working"
    if state == "ready" and member.get("busy"):
        return "working"
    if state == "ready":
        return "idle"
    if state == "done" and run in ("stopped", "cancelled"):
        return "stopped"
    if state == "done":
        return "done"
    if state == "failed":
        return "failed"
    if run == "paused":
        return "paused"
    return state.replace("_", " ") if state else "unknown"


def needs_you_list(db, project, lead_state, crew, factory_rows):
    """Every human gate of a project, one item each: the Captain's own question, paused runs,
    draft or changed project factories, pending writer-machine grants, blocked crew threads,
    and missing worktrees. The project rail's count is its length."""
    items = []
    if lead_state == "needs_you":
        items.append({"kind": "captain", "id": project["lead"], "label": "captain"})
    for m in crew:
        label = _member_label(m)
        if m.get("worktree_missing"):
            items.append({"kind": "worktree_missing", "id": m["id"], "member": m["id"], "label": label,
                          "detail": f"Worktree missing at {m['cwd']}"})
        elif m["kind"] == "factory" and (m.get("run_state") == "paused" or m["state"] in ("needs_you", "failed", "unknown")):
            item = {"kind": "paused_run", "id": m["id"], "member": m["id"], "label": label,
                    "state": "paused" if m.get("run_state") == "paused" else m["state"],
                    "detail": (m.get("detail") or "")[-300:]}
            if m.get("step"):
                item["step"] = m["step"]
            items.append(item)
        elif m["kind"] != "factory" and m["state"] in ("needs_you", "failed", "unknown"):
            items.append({"kind": "blocked_crew", "id": m["id"], "member": m["id"], "label": label,
                          "state": m["state"], "detail": (m.get("detail") or "")[-300:]})
    for f in factory_rows:
        if f["state"] in ("draft", "changed"):
            items.append({"kind": f["state"] + "_factory", "id": project["id"] + "/" + f["name"],
                          "factory": f["name"], "label": "factory " + f["name"], "digest": f["digest"]})
        elif f.get("drift", "").startswith("specialist"):
            items.append({"kind": "changed_factory", "id": project["id"] + "/" + f["name"],
                          "factory": f["name"], "label": "factory " + f["name"], "detail": f["drift"]})
    for g in db.execute(
        "SELECT * FROM pending_grants WHERE project=? ORDER BY created_at,id", (project["id"],)
    ):
        items.append({"kind": "pending_grant", "id": g["id"], "grant_id": g["id"], "machine": g["machine"],
                      "label": "grant · " + g["machine"], "detail": g["task"][:300], "created_at": g["created_at"]})
    for d in open_decisions(db, project["id"]):
        items.append({"kind": "decision", "id": d["id"], "decision_id": d["id"], "member": d["member"],
                      "label": "decision · " + d["title"], "title": d["title"], "summary": d["summary"],
                      "recommendation": d["recommendation"], "options": d["options"], "created_at": d["created_at"]})
    budget = budget_state(db, project)
    if budget["exceeded"]:
        spent = [f"${budget['spent_usd']:.2f} of ${budget['budget_usd']:.2f}"] if budget["budget_usd"] is not None else []
        if budget["budget_tokens"] is not None:
            spent.append(f"{budget['spent_tokens']} of {budget['budget_tokens']} tokens")
        items.append(dict(budget, kind="budget_exceeded", id=project["id"] + "/budget", label="budget",
                          detail="Budget spent: " + ", ".join(spent) + ". Automated turns wait until you extend it."))
    return items


def _status_payload(db, project, factories=None, factory_update=None, factory_removed=None):
    pid = project["id"]
    crew = [
        dict(row)
        for row in db.execute(
            "SELECT c.*,m.state AS run_state,m.job AS run_job FROM crew c LEFT JOIN machines m ON m.member=c.id WHERE c.project=? AND c.archived=0 ORDER BY c.updated DESC,c.id",
            (pid,),
        )
    ]
    busy = {
        r[0]
        for r in db.execute(
            "SELECT member FROM jobs WHERE project=? AND status IN ('queued','running')",
            (pid,),
        )
    }
    latest = db.execute(
        "SELECT status,result FROM jobs WHERE member=? ORDER BY created DESC LIMIT 1",
        (project["lead"],),
    ).fetchone()
    lead_state = (
        "running"
        if latest and latest[0] in ("queued", "running")
        else "needs_you"
        if latest and latest[0] in ("needs_you", "interrupted")
        else "ready"
    )
    approvals = {
        r["name"]: r["digest"]
        for r in db.execute(
            "SELECT name,digest FROM factory_approvals WHERE project=?", (pid,)
        )
    }
    if factories is None:
        existing = read_json(STATUS_DIR / f"{pid}.json") or {}
        raw_factories = existing.get("factories") or []
        by_name = {}
        for item in raw_factories:
            if not isinstance(item, dict) or not item.get("name"):
                continue
            fname = str(item["name"])
            fstate = str(item.get("state") or "")
            if fname in approvals and fstate in ("draft", "changed"):
                if item.get("digest") and approvals[fname] == item.get("digest"):
                    fstate = "approved"
            by_name[fname] = {
                "name": fname,
                "state": fstate,
                "digest": str(item.get("digest") or ""),
                "drift": str(item.get("drift") or ""),
            }
        if factory_removed:
            by_name.pop(factory_removed, None)
        if isinstance(factory_update, dict) and factory_update.get("name"):
            fname = str(factory_update["name"])
            if factory_update.get("state") == "missing":
                by_name.pop(fname, None)
            else:
                by_name[fname] = {
                    "name": fname,
                    "state": str(factory_update.get("state") or ""),
                    "digest": str(factory_update.get("digest") or ""),
                    # A row without lock rows (invalid, or no modules.lock) has no drift.
                    "drift": str(factory_update.get("drift") or ""),
                }
        factory_rows = [by_name[k] for k in sorted(by_name)]
    else:
        factory_rows = [
            {
                "name": str(f.get("name") or ""),
                "state": str(f.get("state") or ""),
                "digest": str(f.get("digest") or ""),
                "drift": str(f.get("drift") or ""),
            }
            for f in factories
            if isinstance(f, dict) and f.get("name")
        ]

    working = 1 if lead_state == "running" else 0
    needs_you = 1 if lead_state == "needs_you" else 0
    factory_running = 0
    factory_paused = 0
    active_needs = []
    active_working = []
    toast_states = {}
    unread_count = 0
    unread = False

    if lead_state == "needs_you":
        active_needs.append(
            {
                "id": project["lead"],
                "label": "captain",
                "kind": "lead",
                "state": "needs_you",
                "state_text": "needs you",
            }
        )
    elif lead_state == "running":
        active_working.append(
            {
                "id": project["lead"],
                "label": "captain",
                "kind": "lead",
                "state": "running",
                "state_text": "working",
            }
        )

    for member in crew:
        if member["kind"] == "factory":
            if member.get("run_state") in MACHINE_END:
                # A finished run shows its end state; its graph cannot change any more.
                member.update(machine=member["title"].split(" · ", 1)[0], step=member["run_state"])
            else:
                member.update(run_summary(member))
        member["busy"] = member["id"] in busy
        member.update(crew_presence(db, member))
        unread_count += member["unread_count"]
        unread = unread or member["unread"]
        label = _member_label(member)
        state = member["state"]
        run_state = member.get("run_state") or ""
        state_text = _member_state_text(member)
        toast_states[member["id"]] = {
            "label": label,
            "display_name": member["display_name"],
            "state": state,
            "kind": member["kind"],
            "unread": member["unread"],
            "unread_count": member["unread_count"],
            "steerable": member["steerable"],
        }
        is_factory = member["kind"] == "factory"
        if is_factory:
            if run_state == "paused":
                factory_paused += 1
            elif run_state in ("queued", "starting", "running", "stopping") or (
                state == "running" and run_state not in MACHINE_END
            ):
                factory_running += 1
            if state in ("needs_you", "failed", "unknown") or run_state == "paused":
                needs_you += 1
                item = {
                    "id": member["id"],
                    "label": label,
                    "kind": "factory",
                    "state": "paused" if run_state == "paused" else state,
                    "state_text": state_text,
                }
                if member.get("step"):
                    item["step"] = member["step"]
                active_needs.append(item)
            elif run_state in ("queued", "starting", "running", "stopping") or (
                state == "running" and run_state not in MACHINE_END
            ):
                item = {
                    "id": member["id"],
                    "label": label,
                    "kind": "factory",
                    "state": "running",
                    "state_text": state_text,
                }
                if member.get("step"):
                    item["step"] = member["step"]
                active_working.append(item)
        else:
            if state in ("needs_you", "failed", "unknown"):
                needs_you += 1
                active_needs.append(
                    {
                        "id": member["id"],
                        "label": label,
                        "kind": member["kind"],
                        "state": state,
                        "state_text": state_text,
                    }
                )
            elif state == "running" or member["busy"]:
                working += 1
                active_working.append(
                    {
                        "id": member["id"],
                        "label": label,
                        "kind": member["kind"],
                        "state": "running",
                        "state_text": "working",
                    }
                )

    for factory in factory_rows:
        fname = factory["name"]
        fstate = factory["state"]
        toast_states[f"{pid}/{fname}"] = {
            "label": f"factory {fname}",
            "state": fstate,
            "kind": "project_factory",
        }
        if fstate in ("draft", "changed"):
            needs_you += 1
            active_needs.append(
                {
                    "id": f"{pid}/{fname}",
                    "label": f"factory {fname}",
                    "kind": "project_factory",
                    "state": fstate,
                    "state_text": "needs review",
                }
            )
        elif factory.get("drift", "").startswith("specialist"):
            # The approved version vendors an older specialist; updating it is an edit.
            needs_you += 1
            active_needs.append(
                {
                    "id": f"{pid}/{fname}",
                    "label": f"factory {fname}",
                    "kind": "project_factory",
                    "state": "drift",
                    "state_text": factory["drift"],
                }
            )

    active = active_needs + active_working
    needs = needs_you_list(db, project, lead_state, crew, factory_rows)
    needs_you = len(needs)
    usage_detail = project_usage_detail(db, project)
    return {
        "project_id": pid,
        "name": project["name"],
        "root": project["root"],
        "state": project.get("state") or "active",
        "lead_state": lead_state,
        "lead_session_id": usage_detail["lead_session_id"],
        "usage": usage_detail["usage"],
        "session_costs": usage_detail["session_costs"],
        "digest_len": len(project.get("digest") or ""),
        "counts": {
            "working": working,
            "needs_you": needs_you,
            "factory_running": factory_running,
            "factory_paused": factory_paused,
            "total_crew": len(crew),
            "unread": unread_count,
        },
        "unread_count": unread_count,
        "unread": unread,
        "active": active,
        "needs_you_items": active_needs[:5],
        "needs_you": needs,
        "factories": factory_rows,
        "toast_states": toast_states,
    }


def publish_project_statuses(
    db,
    project_id=None,
    factories_by_project=None,
    factory_update=None,
    factory_removed=None,
    project_ids=None,
):
    """Write `<hub>/captains/status/index.json` and `<project-id>.json` atomically on change.

    `project_id` or `project_ids` limits the project files written. One publisher at a time
    holds the status lock and reads the store under it, so a slower publisher cannot write
    an older status over a newer one.
    """
    STATUS_DIR.mkdir(mode=0o700, parents=True, exist_ok=True)
    with open(STATUS_DIR / ".lock", "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        _publish_project_statuses(
            db, project_id, factories_by_project, factory_update, factory_removed, project_ids
        )


def _publish_project_statuses(
    db, project_id, factories_by_project, factory_update, factory_removed, project_ids
):
    all_projects = [dict(r) for r in db.execute("SELECT * FROM projects")]
    # At most one Captain per root is not archived; an archived one never shadows it.
    index = {p["root"]: p["id"] for p in all_projects if p["state"] != "archived"}
    index_path = STATUS_DIR / "index.json"
    if read_json(index_path) != index:
        atomic_write_json(index_path, index)
    valid_ids = {p["id"] for p in all_projects}
    for entry in STATUS_DIR.iterdir():
        if entry.suffix == ".json" and entry.name != "index.json" and entry.stem not in valid_ids:
            try:
                entry.unlink()
            except OSError:
                pass
    if project_id is not None:
        targets = [p for p in all_projects if p["id"] == project_id]
    elif project_ids is not None:
        targets = [p for p in all_projects if p["id"] in project_ids]
    else:
        targets = all_projects
    for project in targets:
        pid = project["id"]
        factories = (factories_by_project or {}).get(pid)
        payload = _status_payload(
            db,
            project,
            factories=factories,
            factory_update=factory_update if pid == project_id else None,
            factory_removed=factory_removed if pid == project_id else None,
        )
        status_path = STATUS_DIR / f"{pid}.json"
        prior = read_json(status_path)
        if isinstance(prior, dict):
            prior_cmp = {k: v for k, v in prior.items() if k != "updated_at"}
        else:
            prior_cmp = None
        if prior_cmp != payload:
            payload["updated_at"] = time.time()
            atomic_write_json(status_path, payload)


def write_roots():
    """Publish Captain roots so the chat screen can show a setup hint without Python."""
    with database() as db:
        roots = sorted(
            r[0]
            for r in db.execute(
                "SELECT DISTINCT root FROM projects WHERE state NOT IN ('archived','deleting')"
            )
        )
        publish_project_statuses(db)
    atomic_write_json(BASE / "roots.json", roots)


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
    factory_models = checked_factory_models(args.get("factory_models"))
    lead = thread["id"] if thread else str(uuid.uuid4())
    with database(True) as db:
        # An archived Captain keeps its root; a new Captain starts beside it.
        existing = db.execute(
            "SELECT * FROM projects WHERE root=? AND state!='archived'", (root,)
        ).fetchone()
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
                factory_models=factory_models,
            )
            db.execute(
                "INSERT INTO projects(id,root,name,objective,lead,parallel,provider,model,thinking,factory_models) VALUES(?,?,?,?,?,?,?,?,?,?)",
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
    compose = args.get("compose")
    if isinstance(compose, dict):
        # The user built the project's factory from the chart in setup; it still needs approval.
        try:
            row = compose_factory(p, compose.get("name") or "pipeline", compose.get("stages"), compose.get("models"))
            result["factory_composed"] = {k: row.get(k) for k in ("name", "state", "digest", "errors")}
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            result["factory_error"] = str(error)
    if args.get("machine"):
        # The user picked the starting factory in setup: that choice approves this one run.
        try:
            with database() as db:
                p = record(db, "projects", p["id"])
            started = dispatch_action(p, "delegate", None, {
                "machine": args["machine"], "task": p["objective"], "approved": True,
                "request_id": "setup/" + args["machine"],
            })
            result["factory_run"] = started
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
