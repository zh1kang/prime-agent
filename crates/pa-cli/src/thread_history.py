"""Read the local conversation indexes. No provider connection is required.

`pa thread history [QUERY]` prints the list. pa's native history panel calls
`pa thread history --json` for the rows (with `rawBytes` and the `cachedTokens` of an absorbed
digest, for cost previews) and `pa thread history --open` (target JSON on stdin) for the path of
a Prime session it can resume. Other providers' chats are absorbed (`pa thread absorb`)."""
import datetime
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import sys
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
            "cursor": "Cursor",
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


def cursor_history(root):
    for log in root.glob("projects/*/agent-transcripts/*.jsonl"):
        try:
            cid = str(uuid.UUID(log.stem))
            title = ""
            with log.open("rb") as handle:
                first = json.loads(handle.readline() or b"{}")
            content = (first.get("message") or {}).get("content") or first.get("content") or ""
            if isinstance(content, list):
                content = " ".join(part.get("text", "") for part in content if isinstance(part, dict))
            title = str(content).replace("<user_query>", "").replace("</user_query>", "")
            add("Cursor", cid, title[:200] or "Untitled thread", log.stat().st_mtime, ("cursor", cid, str(log)))
        except (OSError, ValueError, AttributeError) as err:
            errors.append(f"cursor: {err}")


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
try:
    cursor_history(home / ".cursor")
except OSError as err:
    errors.append(f"cursor: {err}")
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
    """Only Prime sessions resume natively; other chats are absorbed into the current one."""
    if provider == "prime":
        return Path(selector).resolve(strict=True)
    raise ValueError(f"{provider} chats are absorbed, not resumed: use /absorb or "
                     f"`pa thread absorb {provider} {location if provider != 't3' else selector}`")


def source_file(provider, selector, location):
    """The transcript file `pa thread absorb` reads for a row, or None (T3 lives in SQLite)."""
    if provider == "pa":
        return Path(location) / "threads" / (selector + ".json")
    if provider == "t3":
        return None
    return Path(selector if provider == "prime" else location).resolve()


def cost(target):
    """`rawBytes` on disk and `cachedTokens` of an unfocused digest that is still current."""
    provider, selector, location = target
    path = source_file(provider, selector, location)
    if path is None or not path.is_file():
        return 0, None
    stat = path.stat()
    key = hashlib.sha256(f"{provider}\0{path}".encode()).hexdigest()
    pointer = prime / "absorbed" / "sources" / (key + ".json")
    try:
        record = json.loads(pointer.read_bytes())
        if (int(record["mtimeNs"]) == stat.st_mtime_ns and record["sizeBytes"] == stat.st_size
                and (prime / "absorbed" / record["key"] / "digest.json").is_file()):
            return stat.st_size, record["tokens"]
    except (OSError, ValueError, KeyError, TypeError):
        pass
    return stat.st_size, None


def row_json(title, search, target):
    raw, cached = cost(target)
    return {"title": title, "search": search, "target": list(target), "rawBytes": raw, "cachedTokens": cached}


if mode == "--open":
    request = json.load(sys.stdin)
    try:
        path = native_session(*request["target"], request.get("title") or "")
    except (OSError, ValueError, KeyError, TypeError) as err:
        sys.exit("Could not resume: " + clean(err))
    print(json.dumps({"path": str(path)}))
elif mode == "--json":
    print(json.dumps({
        "groups": [
            {"name": name, "rows": [row_json(t, q, target) for t, q, target in groups[name]]}
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
