"""Cron schedules that queue Captain turns, GitHub PR and CI watches on crew rows, and the
escalation decision cards the user answers with one key."""

# minute, hour, day of month, month, day of week (0 or 7 is Sunday).
CRON_FIELDS = ((0, 59), (0, 23), (1, 31), (1, 12), (0, 7))
CRON_SEARCH_DAYS = 366 * 5
SCHEDULE_OPS = ("schedule-add", "schedule-list", "schedule-toggle", "schedule-delete")
DECISION_OPS = ("escalate", "decision-resolve")
WAVE2_OPS = SCHEDULE_OPS + DECISION_OPS + ("pr-watch",)
MAX_DECISION_OPTIONS = 6
PR_POLL_SECONDS = 60
GH_TIMEOUT = 5
_pr_polled_at = 0.0


def cron_values(text, low, high):
    values = set()
    for part in text.split(","):
        body, _, step = part.partition("/")
        if step and not step.isdecimal() or step == "0" or not body:
            raise ValueError(f"Invalid cron field {text!r}.")
        if body == "*":
            first, last = low, high
        elif "-" in body:
            first, _, last = body.partition("-")
            if not (first.isdecimal() and last.isdecimal()):
                raise ValueError(f"Invalid cron range {body!r}.")
            first, last = int(first), int(last)
        elif body.isdecimal():
            first = int(body)
            last = high if step else first
        else:
            raise ValueError(f"Invalid cron field {text!r}.")
        if not low <= first <= last <= high:
            raise ValueError(f"Cron field {text!r} is outside {low}-{high}.")
        values.update(range(first, last + 1, int(step or 1)))
    return values


def parse_cron(expr):
    """(minutes, hours, days of month, months, days of week, dom restricted, dow restricted)."""
    fields = str(expr or "").split()
    if len(fields) != 5:
        raise ValueError("Write a cron expression with five fields: minute hour day-of-month month day-of-week.")
    sets = [cron_values(f, low, high) for f, (low, high) in zip(fields, CRON_FIELDS)]
    sets[4] = {d % 7 for d in sets[4]}
    return (*sets, fields[2] != "*", fields[4] != "*")


def cron_day_matches(spec, moment):
    _, _, days, _, weekdays, dom_set, dow_set = spec
    dom = moment.day in days
    dow = (moment.weekday() + 1) % 7 in weekdays
    if dom_set and dow_set:
        return dom or dow
    return dom if dom_set else dow if dow_set else True


def next_cron_time(expr, after_dt):
    """The first local time after `after_dt` that matches the five-field cron `expr`, as an
    aware datetime. A naive `after_dt` is local time."""
    spec = parse_cron(expr)
    minutes, hours, _, months, _, _, _ = spec
    moment = after_dt.astimezone().replace(tzinfo=None, second=0, microsecond=0)
    moment += datetime.timedelta(minutes=1)
    limit = moment + datetime.timedelta(days=CRON_SEARCH_DAYS)
    while moment < limit:
        if moment.month not in months:
            year, month = divmod(moment.month, 12)
            moment = moment.replace(year=moment.year + year, month=month + 1, day=1, hour=0, minute=0)
        elif not cron_day_matches(spec, moment):
            moment = moment.replace(hour=0, minute=0) + datetime.timedelta(days=1)
        elif moment.hour not in hours:
            moment = moment.replace(minute=0) + datetime.timedelta(hours=1)
        elif moment.minute not in minutes:
            moment += datetime.timedelta(minutes=1)
        else:
            return moment.astimezone()
    raise ValueError(f"The cron expression {expr!r} never matches.")


def next_run_iso(expr, after=None):
    after = after or datetime.datetime.now(datetime.timezone.utc)
    return next_cron_time(expr, after).astimezone(datetime.timezone.utc).isoformat()


def wave2_action(op, p, args):
    if op in SCHEDULE_OPS:
        return schedule_action(op, p, args)
    if op in DECISION_OPS:
        return decision_action(op, p, args)
    return pr_watch(p, args)


def schedule_action(op, p, args):
    actor = actor_kind()
    if actor == "crew":
        raise ValueError("A crew thread cannot manage schedules.")
    if op == "schedule-delete" and actor != "user":
        raise ValueError("Only you delete a schedule.")
    if op in ("schedule-add", "schedule-toggle") and actor == "captain" and captain_turn(p) != "lead":
        raise ValueError("Change schedules only in a turn that answers a user message.")
    if op == "schedule-list":
        with database() as db:
            return {"ok": True, "schedules": project_schedules(db, p["id"])}
    if op == "schedule-add":
        require_active(p)
        title = checked_text(str(args.get("title") or ""), 180)
        expr = " ".join(str(args.get("cron_expr") or "").split())
        task = checked_text(args.get("task"))
        next_run = next_run_iso(expr)
        sid = str(uuid.uuid4())
        with database(True) as db:
            db.execute(
                "INSERT INTO schedules(id,project,title,cron_expr,task,enabled,next_run_at,created_at) VALUES(?,?,?,?,?,1,?,?)",
                (sid, p["id"], title, expr, task, next_run, iso_now()),
            )
            record_edit(db, p["id"], "schedule " + sid[:8], "", expr + " " + title, actor)
        start_watch()
        return {"ok": True, "id": sid, "next_run_at": next_run, "by": actor}
    sid = str(args.get("id") or "")
    with database(True) as db:
        row = db.execute("SELECT * FROM schedules WHERE id=? AND project=?", (sid, p["id"])).fetchone()
        if not row:
            raise ValueError("That schedule does not exist in this project.")
        if op == "schedule-delete":
            db.execute("DELETE FROM schedules WHERE id=?", (sid,))
            record_edit(db, p["id"], "schedule " + sid[:8], row["cron_expr"], "", actor)
            return {"ok": True, "id": sid, "deleted": True}
        enabled = args.get("enabled")
        if not isinstance(enabled, bool):
            raise ValueError("Pass enabled as true or false.")
        next_run = next_run_iso(row["cron_expr"]) if enabled else ""
        db.execute("UPDATE schedules SET enabled=?,next_run_at=? WHERE id=?", (int(enabled), next_run, sid))
    if enabled:
        start_watch()
    return {"ok": True, "id": sid, "enabled": enabled, "next_run_at": next_run}


def project_schedules(db, project):
    return [
        dict(r, enabled=bool(r["enabled"]))
        for r in db.execute("SELECT * FROM schedules WHERE project=? ORDER BY created_at,id", (project,))
    ]


def fire_due_schedules(db):
    """Queue one lead turn per due schedule on the normal job queue and advance its next run.
    A schedule of a stopped project, or of one over its budget, waits. Returns the job IDs."""
    now = time.time()
    jobs = []
    for row in db.execute(
        "SELECT s.*,p.lead,p.state FROM schedules s JOIN projects p ON p.id=s.project "
        "WHERE s.enabled=1 AND s.next_run_at!=''"
    ).fetchall():
        due = parse_iso(row["next_run_at"])
        if due is None or due > now or (row["state"] or "active") != "active":
            continue
        p = record(db, "projects", row["project"])
        if budget_state(db, p)["exceeded"]:
            continue
        try:
            following = next_run_iso(row["cron_expr"])
        except ValueError:
            following = ""
        jid = queue(db, row["project"], row["lead"], "lead", row["task"],
                    "schedule/" + row["id"] + "/" + row["next_run_at"])
        db.execute(
            "UPDATE jobs SET request=? WHERE id=? AND request IS NULL",
            (json.dumps(["lead", {"text": row["task"], "source": "schedule", "schedule": row["id"]}], sort_keys=True), jid),
        )
        db.execute(
            "UPDATE schedules SET last_run_at=?,next_run_at=?,enabled=? WHERE id=?",
            (iso_now(), following, int(bool(following)), row["id"]),
        )
        record_notification(db, row["project"], "", "schedule", "Schedule ran: " + row["title"], row["task"][:300])
        jobs.append(jid)
    return jobs


def job_source(j):
    """The source a job's request names, such as schedule, or ""."""
    try:
        _, args = json.loads((j or {}).get("request") or "null") or (None, {})
    except (ValueError, TypeError):
        return ""
    return str(args.get("source") or "") if isinstance(args, dict) else ""


def gh_enabled():
    return os.getenv("PA_CAPTAIN_DISABLE_GH") != "1" and shutil.which("gh") is not None


def gh_json(cwd, *args):
    run = subprocess.run(
        ["gh", *args], cwd=cwd, env=_git_env, capture_output=True, text=True, timeout=GH_TIMEOUT,
        stdin=subprocess.DEVNULL,
    )
    if run.returncode:
        raise ValueError((run.stderr or run.stdout or "gh failed.").strip()[-400:])
    return json.loads(run.stdout)


def pr_watch(p, args):
    if actor_kind() == "crew":
        raise ValueError("A crew thread cannot watch pull requests.")
    with database() as db:
        m = record(db, "crew", str(args.get("member") or ""))
    if m["project"] != p["id"]:
        raise ValueError("Select a member of this project.")
    url, number = args.get("url"), args.get("number")
    if url is None and number is None:
        if not gh_enabled():
            raise ValueError("Pass url or number; gh is not available to find the pull request.")
        try:
            found = gh_json(m["cwd"], "pr", "view", "--json", "url,number")
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            raise ValueError(f"gh found no pull request for this thread: {error}") from None
        url, number = found.get("url"), found.get("number")
    url = checked_text(str(url), 500) if url else ""
    if number is not None and (isinstance(number, bool) or not str(number).isdecimal()):
        raise ValueError("Pass number as a pull request number.")
    number = int(number) if number is not None else None
    if not url and number is None:
        raise ValueError("Pass url or number.")
    with database(True) as db:
        db.execute("UPDATE crew SET pr_url=?,pr_number=?,updated=? WHERE id=?", (url, number, time.time(), m["id"]))
    pr_cache_path(m["id"]).unlink(missing_ok=True)
    start_watch()
    return {"ok": True, "member": m["id"], "pr_url": url, "pr_number": number}


def pr_cache_path(member):
    return BASE / "pr-cache" / (safe_handle(member) + ".json")


def pr_status(member):
    """The last observed PR pill of a crew row, read from its cache, or None."""
    cached = read_json(pr_cache_path(member["id"])) if member.get("pr_url") or member.get("pr_number") else None
    return cached.get("pill") if isinstance(cached, dict) else None


def pr_checks(rollup):
    states = []
    for check in rollup or []:
        if not isinstance(check, dict):
            continue
        conclusion = str(check.get("conclusion") or check.get("state") or "").upper()
        status = str(check.get("status") or "").upper()
        if conclusion in ("FAILURE", "ERROR", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STARTUP_FAILURE"):
            states.append("fail")
        elif status and status != "COMPLETED" or conclusion in ("PENDING", "EXPECTED", ""):
            states.append("pending")
        else:
            states.append("pass")
    return "fail" if "fail" in states else "pending" if "pending" in states else "pass"


def pr_watch_rows(db):
    """Crew rows whose pull request is open as far as pa last saw."""
    rows = db.execute(
        "SELECT c.* FROM crew c JOIN projects p ON p.id=c.project "
        "WHERE (c.pr_url!='' OR c.pr_number IS NOT NULL) AND c.archived=0 AND p.state='active'"
    ).fetchall()
    return [dict(r) for r in rows if (pr_status(dict(r)) or {}).get("state") not in ("MERGED", "CLOSED")]


def pr_event(pill, number):
    """(crew state, wake event, text) of a PR change worth a report, or None."""
    name = f"PR #{number}" if number else "The pull request"
    if pill["state"] == "MERGED":
        return "done", "done", f"{name} was merged."
    if pill["state"] == "CLOSED":
        return "done", "done", f"{name} was closed without merging."
    if pill["conflict"]:
        return "needs_you", "needs_you", f"NEEDS YOU: {name} has merge conflicts with its base branch."
    if pill["checks"] == "fail":
        return "failed", "failed", f"{name} has failing CI checks."
    if pill["review_decision"] == "CHANGES_REQUESTED":
        return "needs_you", "needs_you", f"NEEDS YOU: {name} has review changes requested."
    if pill["checks"] == "pass" and pill["review_decision"] == "APPROVED":
        return "done", "done", f"{name} is approved and its checks pass."
    return None


def poll_pr_watches(db, force=False):
    """Check watched pull requests at most every PR_POLL_SECONDS and report a change of CI,
    review, conflict, or merge state once. Returns the number of reports."""
    global _pr_polled_at
    if not force and time.time() - _pr_polled_at < PR_POLL_SECONDS or not gh_enabled():
        return 0
    rows = pr_watch_rows(db)
    if not rows:
        return 0
    _pr_polled_at = time.time()
    reports = 0
    for m in rows:
        target = m["pr_url"] or str(m["pr_number"])
        try:
            data = gh_json(m["cwd"] if Path(m["cwd"]).is_dir() else BASE, "pr", "view", target, "--json",
                           "state,mergeStateStatus,statusCheckRollup,reviewDecision,comments")
        except (OSError, ValueError, subprocess.SubprocessError):
            continue
        pill = {
            "state": str(data.get("state") or "").upper(),
            "checks": pr_checks(data.get("statusCheckRollup")),
            "review_decision": str(data.get("reviewDecision") or ""),
            "conflict": str(data.get("mergeStateStatus") or "").upper() == "DIRTY",
            "comments": len(data.get("comments") or []),
            "url": m["pr_url"],
            "number": m["pr_number"],
            "checked_at": iso_now(),
        }
        fingerprint = hashlib.sha256(json.dumps(
            [pill[k] for k in ("state", "checks", "review_decision", "conflict", "comments")]
        ).encode()).hexdigest()[:16]
        prior = read_json(pr_cache_path(m["id"])) or {}
        atomic_write_json(pr_cache_path(m["id"]), {"fingerprint": fingerprint, "pill": pill})
        event = pr_event(pill, m["pr_number"]) if prior.get("fingerprint") != fingerprint else None
        if event:
            state, kind, text = event
            report(db, record(db, "projects", m["project"]), m["id"], "pr/" + m["id"] + "/" + fingerprint,
                   state, text, kind=kind)
            reports += 1
    return reports


def checked_options(value):
    if not isinstance(value, list) or not 1 <= len(value) <= MAX_DECISION_OPTIONS:
        raise ValueError(f"Pass options as a list of 1 to {MAX_DECISION_OPTIONS} choices.")
    options = []
    for i, item in enumerate(value):
        if not isinstance(item, dict):
            raise ValueError("Pass each option as {key, label, reply_text}.")
        key = str(item.get("key") or i + 1).strip()
        if not key or len(key) > 8 or key in {o["key"] for o in options}:
            raise ValueError("Give each option a short unique key, such as 1, 2, or 3.")
        options.append({
            "key": key,
            "label": checked_text(str(item.get("label") or ""), 200),
            "reply_text": checked_text(str(item.get("reply_text") or ""), 4000),
        })
    return options


def decision_row(r):
    return dict(r, options=json.loads(r["options_json"] or "[]"))


def open_decisions(db, project):
    return [
        decision_row(r)
        for r in db.execute(
            "SELECT * FROM decisions WHERE project=? AND status='open' ORDER BY created_at,id", (project,)
        )
    ]


def decision_action(op, p, args):
    actor = actor_kind()
    if actor == "crew":
        raise ValueError("A crew thread cannot escalate to the user.")
    if op == "escalate":
        title = checked_text(str(args.get("title") or ""), 200)
        summary = checked_text(str(args.get("summary") or ""), 4000)
        recommendation = str(args.get("recommendation") or "").strip()[:2000]
        options = checked_options(args.get("options"))
        member = str(args.get("member") or "")
        did = str(uuid.uuid4())
        with database(True) as db:
            if member and not db.execute("SELECT 1 FROM crew WHERE id=? AND project=?", (member, p["id"])).fetchone():
                raise ValueError("Select a member of this project.")
            db.execute(
                "INSERT INTO decisions(id,project,member,title,summary,recommendation,options_json,status,created_at) "
                "VALUES(?,?,?,?,?,?,?,'open',?)",
                (did, p["id"], member, title, summary, recommendation, json.dumps(options), iso_now()),
            )
            record_notification(db, p["id"], member, "decision", "Decision: " + title, summary)
        return {"ok": True, "id": did, "options": options, "by": actor}
    if actor != "user":
        raise ValueError("Only you resolve a decision.")
    did = str(args.get("id") or "")
    with database() as db:
        row = db.execute("SELECT * FROM decisions WHERE id=? AND project=?", (did, p["id"])).fetchone()
    if not row:
        raise ValueError("That decision does not exist in this project.")
    if row["status"] != "open":
        return {"ok": True, "id": did, "status": row["status"], "chosen_key": row["chosen_key"], "repeat": True}
    options = json.loads(row["options_json"] or "[]")
    key = str(args.get("key") or "")
    custom = str(args.get("reply_text") or "").strip()
    chosen = next((o for o in options if o["key"] == key), None)
    if key and not chosen:
        raise ValueError("Choose one of the keys: " + ", ".join(o["key"] for o in options) + ".")
    if not chosen and not custom:
        raise ValueError("Choose an option key or write a reply.")
    text = custom or chosen["reply_text"]
    require_active(p)
    started = dispatch_action(p, "lead", None, {
        "text": f"Decision on {row['title']}: {text}", "request_id": "decision/" + did,
    })
    with database(True) as db:
        db.execute(
            "UPDATE decisions SET status='resolved',chosen_key=?,resolved_at=? WHERE id=? AND status='open'",
            (chosen["key"] if chosen else "", iso_now(), did),
        )
    return {"ok": True, "id": did, "status": "resolved", "chosen_key": chosen["key"] if chosen else "",
            "job": started["job"]}
