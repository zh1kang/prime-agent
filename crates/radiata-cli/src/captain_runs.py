"""Factory run addresses and summaries, and the names the panel shows for crew rows."""

import re

DISPLAY_TASK_CHARS = 48
# Crew kinds that are not a machine name, so a display name does not lead with them.
GENERIC_KINDS = ("", "thread", "native", "factory")


BARE_ID = re.compile(r"(?:[0-9a-f]{8}|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})", re.I)


def is_bare_id(text):
    """Whether `text` is only a member or session ID such as `d0e6edff`."""
    return bool(BARE_ID.fullmatch(str(text or "").strip()))


def make_display_name(machine_or_kind, title, task, fallback_id):
    """A short human name such as `eigen-pipeline · Phase 2 classical baselines`: the machine,
    then the title or the start of the task. Never the bare member ID."""
    lead = " ".join(str(machine_or_kind or "").split())
    if lead in GENERIC_KINDS:
        lead = ""
    text = " ".join(str(title or "").split())
    if lead and text.startswith(lead + " · "):
        text = text[len(lead) + 3:]
    if is_bare_id(text) or text == str(fallback_id or ""):
        text = ""
    if not text:
        text = " ".join(str(task or "").split())
        if len(text) > DISPLAY_TASK_CHARS:
            text = text[:DISPLAY_TASK_CHARS].rsplit(" ", 1)[0].rstrip(" ,;:.") + "…"
    if lead and text:
        return (lead + " · " + text)[:180]
    if lead or text:
        return (lead or text)[:180]
    return "factory run" if machine_or_kind == "factory" else "crew thread"


def member_display_name(member):
    """The stored display name of a crew row, else one made from its machine and title."""
    if member.get("display_name") and not is_bare_id(member["display_name"]):
        return member["display_name"]
    title = str(member.get("title") or "")
    machine = title.split(" · ", 1)[0] if member.get("kind") == "factory" else member.get("kind")
    return make_display_name(machine, title, "", member.get("id"))


# Step agents by status: the working ones first, then the ones that wait, then the settled.
STEP_ORDER = {"running": 0, "queued": 1, "waiting": 2, "paused": 2, "error": 3, "failed": 3}
# The daemon roster is read at most once per this many seconds, so one status read across
# many factory rows asks the socket once.
ROSTER_TTL = 2.0
_roster_cache = {"at": 0.0, "rows": None}


def daemon_roster():
    """The daemon's session rows, cached briefly; empty when no daemon answers."""
    now = time.time()
    if _roster_cache["rows"] is not None and now - _roster_cache["at"] < ROSTER_TTL:
        return _roster_cache["rows"]
    rows = []
    if daemon_socket():
        try:
            listed = daemon_request({"type": "list", "includeClientOwned": True}, timeout=2)
            rows = [r for r in (listed.get("data") or {}).get("sessions") or [] if isinstance(r, dict)]
        except (OSError, ValueError):
            rows = []
    _roster_cache.update(at=now, rows=rows)
    return rows


def _either(row, snake, camel):
    value = row.get(snake)
    return row.get(camel) if value is None else value


def _session_tree(rows, host):
    """The realpaths of `host` and every session the roster says descends from it."""
    tree = {os.path.realpath(host)} if host else set()
    grew = True
    while grew:
        grew = False
        for row in rows:
            parent = row.get("parentSessionPath") or row.get("parentSessionFile")
            path = row.get("sessionFile") or row.get("sessionPath")
            if parent and path and os.path.realpath(parent) in tree and os.path.realpath(path) not in tree:
                tree.add(os.path.realpath(path))
                grew = True
    return tree


def _live_child(rows, tree, child_id):
    """The roster row of the subagent `child_id` under the run's host session tree."""
    for row in rows:
        if row.get("rlmChildId") != child_id:
            continue
        parent = row.get("parentSessionPath") or row.get("parentSessionFile")
        if not tree or not parent or os.path.realpath(parent) in tree:
            return row
    return None


def _session_from_dir(session_dir):
    """The newest session file in a child's session directory, if it exists on disk."""
    if not session_dir:
        return ""
    path = Path(str(session_dir)).expanduser()
    if path.is_file():
        return str(path)
    try:
        files = sorted(path.glob("*.jsonl"), key=lambda p: p.stat().st_mtime)
    except OSError:
        return ""
    return str(files[-1]) if files else ""


def step_agents(graph, host_session):
    """One row per subagent a run's states spawned, from the instance rows of its graph (and a
    node's own child fields, as older graphs carry them), resolved live against the daemon."""
    states = {}
    for state in (graph.get("machine") or {}).get("states") or []:
        if isinstance(state, dict) and state.get("id"):
            states[state["id"]] = state
    rows = None
    tree = None
    agents = []
    for node in graph.get("nodes") or []:
        if not isinstance(node, dict):
            continue
        state = states.get(node.get("id")) or {}
        sub = state.get("subagent") if isinstance(state.get("subagent"), dict) else {}
        base = {
            "state": node.get("id") or "",
            "role": node.get("role") or state.get("role") or sub.get("role") or "",
            "model": node.get("model") or state.get("model") or sub.get("model") or "",
            "thinking": node.get("thinking") or state.get("thinking") or sub.get("thinking") or "",
        }
        instances = [i for i in node.get("instances") or [] if isinstance(i, dict) and i.get("child")]
        if not instances:
            child = next(
                (node[k] for k in ("child", "child_id", "childId") if isinstance(node.get(k), str) and node[k]),
                None,
            )
            if child:
                instances = [{"child": child, "status": node.get("status") or ""}]
        for inst in instances:
            child = inst["child"]
            if rows is None:
                rows = daemon_roster()
                tree = _session_tree(rows, host_session)
            live = _live_child(rows, tree, child) or {}
            session = live.get("sessionFile") or live.get("sessionPath") or _session_from_dir(
                _either(inst, "session_dir", "sessionDir")
            )
            agent = dict(
                base,
                status=inst.get("status") or node.get("status") or "",
                child_id=child,
                child_name=_either(inst, "child_name", "childName") or live.get("name") or "",
                session=session or "",
                session_id=live.get("activeSessionId") or live.get("id") or live.get("sessionId") or "",
                attempt=inst.get("attempt"),
                entry=inst.get("entry"),
                duration_ms=_either(inst, "duration_ms", "durationMs"),
                spawned_at=_either(inst, "spawned_at", "spawnedAt"),
                error=inst.get("error"),
                is_streaming=bool(live.get("isStreaming") or live.get("isRunningTools")),
            )
            note = live.get("activity") or live.get("currentTool") or live.get("progress")
            if isinstance(note, str) and note.strip():
                agent["progress_note"] = " ".join(note.split())[:120]
            agents.append(agent)
    live = [a for a in agents if a["status"] in STEP_ORDER]
    live.sort(key=lambda a: STEP_ORDER[a["status"]])
    settled = [a for a in agents if a["status"] not in STEP_ORDER]
    return live + settled[::-1]


def active_states(graph):
    """The states in flight: the graph's own active list, else the nodes with live instances."""
    listed = graph.get("active_nodes") or graph.get("activeNodes")
    if isinstance(listed, list) and listed:
        return [s for s in listed if isinstance(s, str)]
    active = []
    for node in graph.get("nodes") or []:
        if not isinstance(node, dict) or not node.get("id"):
            continue
        live = node.get("status") in ("running", "waiting") or any(
            isinstance(i, dict) and i.get("status") in ("running", "waiting", "queued")
            for i in node.get("instances") or []
        )
        if live:
            active.append(node["id"])
    return active


def run_address(member):
    """Where a Captain-started run lives, so pa can open it live: its native host session, its
    run ID in that session's kernel, its graph, and the subagents of its steps."""
    if not member.get("run_job"):
        return {}
    control = BASE / "runs" / member["id"]
    run_id = member.get("run_id") or (read_json(control / "run.json") or {}).get("run")
    graph = read_json(control / "graph.json") or {}
    host = member.get("run_session") or member["ref"].split("#", 1)[0]
    nodes = []
    children = []

    def add(child, row):
        if isinstance(child, str) and child and child not in children:
            children.append(child)
            row.setdefault("children", []).append(child)

    for node in graph.get("nodes") or []:
        if not isinstance(node, dict):
            continue
        row = {k: node[k] for k in ("id", "status", "kind") if k in node}
        for key in ("child", "child_id", "childId", "session", "session_id", "children"):
            value = node.get(key)
            for child in value if isinstance(value, list) else [value]:
                add(child, row)
        for inst in node.get("instances") or []:
            if isinstance(inst, dict):
                add(inst.get("child"), row)
        nodes.append(row)
    try:
        for child in json.loads(member.get("run_children") or "[]"):
            if isinstance(child, str) and child not in children:
                children.append(child)
    except ValueError:
        pass
    agents = step_agents(graph, host)
    by_child = {a["child_id"]: a["session"] for a in agents if a["session"]}
    return {
        "host_session": host,
        "run_id": run_id,
        "graph": {"nodes": nodes} if nodes else None,
        "step_sessions": [by_child.get(c, c) for c in children],
        "step_agents": agents,
        "active_states": active_states(graph),
    }


def run_summary(member):
    """Machine name, current state, progress, and need of one factory row."""
    summary = {"machine": member["title"].split(" · ", 1)[0], "headless": bool(member.get("run_job"))}
    graph = read_json(BASE / "runs" / member["id"] / "graph.json") if member.get("run_job") else None
    states = [s for s in (graph or {}).get("nodes") or [] if isinstance(s, dict)]
    if states:
        done = sum(1 for s in states if s.get("status") in ("done", "skipped"))
        summary["progress"] = f"{done}/{len(states)}"
        active = active_states(graph)
        summary["active_states"] = active
        summary["active_count"] = sum(
            1
            for s in states
            for i in s.get("instances") or []
            if isinstance(i, dict) and i.get("status") == "running"
        ) or sum(1 for s in states if s.get("status") == "running")
        stuck = [s["id"] for s in states if s.get("status") == "error" and s.get("id")]
        if member.get("run_state") in MACHINE_END:
            summary["step"] = member["run_state"]
        elif active:
            summary["step"] = " + ".join(active)
        elif stuck:
            summary["step"] = stuck[0]
    return summary
