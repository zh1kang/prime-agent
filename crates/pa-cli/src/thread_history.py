"""Read the local conversation indexes. No provider connection is required.

`pa thread history [QUERY]` prints the list. pa's native history panel calls
`pa thread history --json` for the rows and `pa thread history --open` (target JSON on
stdin) to turn a row into a native session it can resume."""
import datetime
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import subprocess
import uuid
from collections import defaultdict
import unicodedata

home = Path.home()
rows = []
errors = []


def clean(value):
    return " ".join("".join(c if not unicodedata.category(c).startswith("C") else " "
                            for c in str(value or "")).split())


def add(source, thread, title, stamp, target):
    if isinstance(stamp, str):
        stamp = datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
    date = datetime.datetime.fromtimestamp(stamp).strftime("%Y-%m-%d %H:%M")
    group = "T3/Flock" if source.startswith("T3/Flock ") else source
    if source.startswith("pa "):
        group = {
            "prime": "Prime",
            "claude": "Claude",
            "codex": "Codex",
            "antigravity": "Antigravity",
        }.get(source[3:], source)
    title = clean(title) or "Untitled thread"
    rows.append((stamp, group, title, f"{source} {thread} {date} {title}".casefold(), target))


def database(path, codex=False):
    if not path.is_file():
        return
    with sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True) as db:
        if codex:
            columns = {row[1] for row in db.execute("PRAGMA table_info(threads)")}
            title = "coalesce(nullif(name, ''), title)" if "name" in columns else "title"
            for tid, label, stamp, cwd in db.execute(f"SELECT id,{title},updated_at,rollout_path FROM threads"):
                add("Codex", tid, label, stamp, ("codex", tid, cwd))
            return
        tables = {r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        seen = set()
        for table, provider in [("orchestration_v2_projection_threads", "default_provider"),
                                ("projection_threads", "'legacy'")]:
            if table not in tables:
                continue
            for tid, title, stamp, deleted, model in db.execute(
                    f"SELECT thread_id,title,updated_at,deleted_at,{provider} FROM {table}"):
                if tid not in seen and not deleted:
                    add("T3/Flock " + str(model), tid, title, stamp, ("t3", tid, table))
                seen.add(tid)


def transcripts(root, name):
    for path in root.rglob("*.jsonl"):
        if "subagents" in path.parts or path.stem.startswith("agent-"):
            continue
        try:
            title = ""
            tid = path.stem
            cwd = ""
            with path.open("rb") as stream:
                for line in stream.read(65536).splitlines():
                    try:
                        event = json.loads(line)
                    except (ValueError, UnicodeError):
                        continue
                    if event.get("type") == "session" and event.get("id"):
                        tid = event["id"]
                    cwd = event.get("cwd") or cwd
                    msg = event.get("message") or {}
                    if msg.get("role") == "user":
                        content = msg.get("content")
                        title = content if isinstance(content, str) else " ".join(
                            b.get("text", "") for b in content or [] if isinstance(b, dict))
                        break
                # Renames are appended to native transcripts. Check the tail first.
                stream.seek(max(0, path.stat().st_size - 65536))
                for line in reversed(stream.read().splitlines()):
                    if not any(marker in line for marker in (b'custom-title', b'session_info', b'agent-name')):
                        continue
                    try:
                        event = json.loads(line)
                    except (ValueError, UnicodeError):
                        continue
                    field = {"custom-title": "customTitle", "session_info": "name",
                             "agent-name": "agentName"}.get(event.get("type"))
                    if field and event.get(field):
                        title = event[field]
                        break
            add(name, tid, title, path.stat().st_mtime, (name.lower(), str(path) if name == "Prime" else tid, str(path)))
        except OSError as err:
            errors.append(str(err))


def harness(root):
    for path in root.glob("threads/*.json"):
        try:
            thread = json.loads(path.read_bytes())
            title = next((text for role, text in thread["messages"] if role == "user"), "Untitled thread")
            add("pa " + thread["provider"], thread["id"], title, path.stat().st_mtime, ("pa", thread["id"], str(root)))
        except (OSError, ValueError, KeyError) as err:
            errors.append(str(err))


def antigravity_history(root):
    dbpath = root / "conversation_summaries.db"
    if not dbpath.is_file():
        return
    with sqlite3.connect(dbpath.resolve().as_uri() + "?mode=ro", uri=True) as db:
        tables = {r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if "conversation_summaries" not in tables:
            return
        for cid, title, preview, stamp in db.execute(
            "SELECT conversation_id,title,preview,last_modified_time FROM conversation_summaries WHERE parent_conversation_id=''"
        ):
            log = root / "brain" / cid / ".system_generated/logs/transcript.jsonl"
            if not log.is_file():
                continue
            try:
                add("Antigravity", cid, title or preview or "Untitled thread", stamp, ("antigravity", cid, str(log)))
            except ValueError:
                add("Antigravity", cid, title or preview or "Untitled thread", log.stat().st_mtime, ("antigravity", cid, str(log)))


for path, codex in [(home / ".t3/userdata/statev2.sqlite", False),
                    (Path(os.getenv("CODEX_HOME", home / ".codex")) / "state_5.sqlite", True)]:
    try:
        database(path, codex)
    except (OSError, ValueError, sqlite3.Error) as err:
        errors.append(f"{path}: {err}")
try:
    antigravity_history(home / ".gemini/antigravity-cli")
except (OSError, ValueError, sqlite3.Error) as err:
    errors.append(f"antigravity: {err}")
prime = Path(os.getenv("PRIME_AGENT_CODING_AGENT_DIR", home / ".prime/agent"))
roots = {prime, home / ".prime/harness", home / ".prime/agent"}
for root in roots:
    harness(root)
session_dirs = {root / "sessions" for root in roots}
if os.getenv("PRIME_AGENT_SESSION_DIR"):
    session_dirs.add(Path(os.environ["PRIME_AGENT_SESSION_DIR"]).expanduser())
for directory in session_dirs:
    transcripts(directory, "Prime")
transcripts(Path(os.getenv("CLAUDE_CONFIG_DIR", home / ".claude")) / "projects", "Claude")
args = sys.argv[1:]
mode = args.pop(0) if args and args[0] in ("--json", "--open") else ""
query = " ".join(args).casefold()
groups = defaultdict(list)
for _, group, title, search, target in sorted(rows, reverse=True):
    if query in search:
        groups[group].append((title, search, target))
providers = sorted(groups, key=lambda name: (name != "T3/Flock", name))


def native_session(provider, selector, location, title):
    if provider == "prime":
        path = Path(selector).resolve(strict=True)
        return path
    uuid.UUID(selector)
    destination = Path(os.environ.get("PRIME_AGENT_SESSION_DIR", prime / "sessions"))
    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
    sid = str(uuid.uuid5(uuid.NAMESPACE_URL, f"prime-agent/import/{provider}/{selector}"))
    path = destination / (sid + ".jsonl")
    if path.is_file():
        return path
    if provider in ("codex", "claude", "antigravity"):
        data = json.loads(subprocess.check_output([os.environ["PA_HISTORY_EXECUTABLE"], "thread",
            "read-transcript", provider, location], text=True))
        cwd, messages = data["cwd"], data["messages"]
    elif provider == "pa":
        data = json.loads((Path(location) / "threads" / (selector + ".json")).read_bytes())
        cwd, messages = data["cwd"], data["messages"]
    else:
        dbpath = home / ".t3/userdata/statev2.sqlite"
        modern = location == "orchestration_v2_projection_threads"
        msgtable = "orchestration_v2_projection_messages" if modern else "projection_thread_messages"
        column = "json_extract(payload_json,'$.text')" if modern else "text"
        directory = "json_extract(t.payload_json,'$.worktreePath')" if modern else "t.worktree_path"
        with sqlite3.connect(dbpath.as_uri() + "?mode=ro", uri=True) as db:
            messages = list(db.execute(f"SELECT role,{column} FROM {msgtable} WHERE thread_id=? AND role IN ('user','assistant') ORDER BY created_at,rowid", (selector,)))
            row = db.execute(f"SELECT coalesce({directory},p.workspace_root) FROM {location} t LEFT JOIN projection_projects p ON p.project_id=t.project_id WHERE t.thread_id=?", (selector,)).fetchone()
            cwd = row[0] if row else None
    if cwd and not Path(cwd).is_dir():
        raise ValueError("Original worktree is unavailable: " + str(cwd))
    now = datetime.datetime.now(datetime.timezone.utc)
    stamp = now.isoformat()
    millis = int(now.timestamp() * 1000)
    previous = None
    with tempfile.NamedTemporaryFile(mode="w", dir=destination, prefix=".import-", delete=False) as tmp:
        try:
            def write(record):
                print(json.dumps(record, ensure_ascii=False), file=tmp)
            write(dict(type="session", version=3, id=sid, timestamp=stamp, cwd=str(cwd or Path.cwd())))
            for role, body in messages:
                if role not in ("user", "assistant") or not isinstance(body, str) or not body.strip():
                    continue
                msg = dict(role=role, timestamp=millis, content=[dict(type="text", text=body)])
                if role == "assistant":
                    usage = dict(input=0, output=0, cacheRead=0, cacheWrite=0, totalTokens=0,
                                 cost=dict(input=0, output=0, cacheRead=0, cacheWrite=0, total=0))
                    msg.update(api="openai-responses", provider=provider, model="imported", usage=usage, stopReason="stop")
                entry = str(uuid.uuid4())
                write(dict(type="message", id=entry, parentId=previous, timestamp=stamp, message=msg))
                previous = entry
            if previous is None:
                raise ValueError("No saved text messages to resume")
            write(dict(type="session_info", id=str(uuid.uuid4()), parentId=previous, timestamp=stamp, name=title))
            tmp.flush()
            os.fsync(tmp.fileno())
            try:
                os.link(tmp.name, path)
            except FileExistsError:
                pass
        finally:
            os.unlink(tmp.name)
    return path


if mode == "--open":
    request = json.load(sys.stdin)
    try:
        path = native_session(*request["target"], request.get("title") or "")
    except (OSError, ValueError, KeyError, TypeError, sqlite3.Error, subprocess.CalledProcessError) as err:
        sys.exit("Could not resume: " + clean(err))
    print(json.dumps({"path": str(path)}))
elif mode == "--json":
    print(json.dumps({
        "groups": [
            {"name": name, "rows": [{"title": t, "search": q, "target": list(target)} for t, q, target in groups[name]]}
            for name in providers
        ],
        "errors": [clean(error) for error in errors],
    }, ensure_ascii=False))
else:
    for error in errors:
        print(clean(error), file=sys.stderr)
    for provider in providers:
        print(provider)
        for title, _, _ in groups[provider]:
            print("  " + title)
        print()
    if not providers:
        print("No matching threads")
