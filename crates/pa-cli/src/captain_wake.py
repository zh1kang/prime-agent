"""When a crew event wakes the Captain, delayed jobs and schedules, and user notifications."""

# Per event: react (queue a Captain turn), context (no turn; the next Captain turn reads the
# report), or ignore. debounce_seconds groups events into one wake.
WAKE_CHOICES = ("react", "context", "ignore")
WAKE_EVENTS = ("done", "failed", "needs_you", "interrupted", "factory_done", "factory_paused")
# A decision the user owns wakes the Captain at once, whatever the debounce.
URGENT_EVENTS = ("needs_you", "factory_paused")
MAX_DEBOUNCE = 600
DEFAULT_WAKE_POLICY = {
    "done": "react",
    "failed": "react",
    "needs_you": "react",
    "interrupted": "ignore",
    "factory_done": "react",
    "factory_paused": "react",
    "debounce_seconds": 8,
}


def iso_now():
    """The current UTC time as ISO 8601, the format of not_before and next_run_at."""
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def parse_iso(value):
    """Epoch seconds of an ISO 8601 time, or None for an empty or malformed one."""
    try:
        moment = datetime.datetime.fromisoformat(str(value))
    except ValueError:
        return None
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=datetime.timezone.utc)
    return moment.timestamp()


def wake_policy(project_row):
    """The project's wake policy over DEFAULT_WAKE_POLICY."""
    try:
        stored = json.loads((project_row or {}).get("wake_policy") or "{}")
    except (ValueError, TypeError):
        stored = {}
    policy = dict(DEFAULT_WAKE_POLICY)
    if isinstance(stored, dict):
        policy.update({k: v for k, v in stored.items() if k in DEFAULT_WAKE_POLICY})
    return policy


def checked_wake_policy(current, changes):
    """`current` with the valid entries of `changes` applied; an invalid entry is an error."""
    if not isinstance(changes, dict):
        raise ValueError("Pass the wake policy as a JSON object.")
    policy = dict(current)
    for key, value in changes.items():
        if key == "debounce_seconds":
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not 0 <= value <= MAX_DEBOUNCE:
                raise ValueError(f"Set debounce_seconds from 0 to {MAX_DEBOUNCE}.")
            policy[key] = int(value)
        elif key in WAKE_EVENTS:
            if value not in WAKE_CHOICES:
                raise ValueError(f"Set {key} to one of: " + ", ".join(WAKE_CHOICES) + ".")
            policy[key] = value
        else:
            raise ValueError("Wake policy events: " + ", ".join(WAKE_EVENTS + ("debounce_seconds",)) + ".")
    return policy


def evaluate_wake(project_row, event_kind, member_row=None):
    """(react | context | ignore, debounce seconds) for one crew event."""
    policy = wake_policy(project_row)
    choice = policy.get(event_kind, "react")
    if choice not in WAKE_CHOICES:
        choice = "react"
    try:
        debounce = max(0, min(MAX_DEBOUNCE, int(policy.get("debounce_seconds") or 0)))
    except (TypeError, ValueError):
        debounce = DEFAULT_WAKE_POLICY["debounce_seconds"]
    if event_kind in URGENT_EVENTS:
        debounce = 0
    return choice, debounce


def iso_after(seconds):
    """The UTC time `seconds` from now as ISO 8601."""
    moment = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=seconds)
    return moment.isoformat()


def next_pending_timer_delay(db):
    """Seconds until the earliest queued job's not_before or enabled schedule's next_run_at
    (0 when one is due), or None when nothing waits on a timer."""
    times = [
        r[0]
        for r in db.execute(
            "SELECT not_before FROM jobs WHERE status='queued' AND not_before!='' "
            "UNION ALL SELECT next_run_at FROM schedules WHERE enabled=1 AND next_run_at!=''"
        )
    ]
    stamps = [s for s in map(parse_iso, times) if s is not None]
    if not stamps:
        return None
    return max(0.0, min(stamps) - time.time())


def record_notification(db, project, member, kind, title, detail=""):
    """Add one unread notification for the user and return its ID."""
    return db.execute(
        "INSERT INTO notifications(project,member,kind,title,detail,created_at) VALUES(?,?,?,?,?,?)",
        (project, member or "", kind, title[:300], (detail or "")[-4000:], iso_now()),
    ).lastrowid
