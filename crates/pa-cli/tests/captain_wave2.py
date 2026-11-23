"""Captain wave 2: escalation decision cards, budget caps, cron schedules, stacked crew branches
with depends_on, pre-turn git checkpoints and rollback, the review findings store, and PR
watches. Mock claude and gh CLIs stand in for the providers.
Usage: captain_wave2.py PA TEMP"""
import datetime, json, os, pathlib, sqlite3, subprocess, sys, time

P = pathlib.Path
binary = str(P(sys.argv[1]).resolve())
temp = P(sys.argv[2]).resolve()
a = temp / "alpha"; a.mkdir()
hub = temp / "hub"; native = temp / "native"; native.mkdir()
sessions = native / "sessions"; sessions.mkdir()
mock = temp / "bin"; mock.mkdir()
hold = temp / "hold-parent"
# Crew turns in a worktree commit, so a stacked branch has a parent HEAD to start from.
(mock / "claude").write_text(f"""#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys, time, uuid
prompt = sys.stdin.read()
while pathlib.Path({str(hold)!r}).exists() and "PARENT-TASK" in prompt:
    time.sleep(0.1)
if "/captains/worktrees/" in os.getcwd():
    subprocess.run(["git", "-c", "user.name=Mock", "-c", "user.email=m@localhost", "commit", "--allow-empty", "-qm", "mock turn"], check=True)
print(json.dumps({{"type": "result", "session_id": str(uuid.uuid5(uuid.NAMESPACE_URL, os.getcwd())), "result": "Done.", "is_error": False}}))
""")
(mock / "gh").write_text("""#!/usr/bin/env python3
import json, sys
args = sys.argv[1:]
if args[:2] == ["pr", "view"] and "url,number" in args:
    print(json.dumps({"url": "https://github.com/o/r/pull/8", "number": 8}))
elif args[:2] == ["pr", "view"]:
    print(json.dumps({"state": "MERGED", "mergeStateStatus": "CLEAN", "reviewDecision": "APPROVED", "comments": [],
                      "statusCheckRollup": [{"status": "COMPLETED", "conclusion": "SUCCESS"}]}))
else:
    sys.exit(1)
""")
for tool in ("claude", "gh"):
    (mock / tool).chmod(0o755)
(native / "settings.json").write_text(json.dumps(dict(onboardingShown=True, agentTraces=dict(enabled=False))))

env = dict(os.environ, PA_CAPTAIN_HOME=str(hub), PRIME_AGENT_CODING_AGENT_DIR=str(hub), PA_CAPTAIN_NATIVE_HOME=str(native),
           PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"), PRIME_AGENT_SESSION_DIR=str(sessions),
           DO_NOT_TRACK="1", PATH=str(mock) + ":" + os.environ["PATH"])
for key in ("PRIME_AGENT_KERNEL_PYTHON", "PA_CAPTAIN_ACTOR", "PA_CAPTAIN_SESSION", "RLM_SESSION_ID",
            "PA_CAPTAIN_NATIVE_SOCKET", "PA_CAPTAIN_DISABLE_GH"):
    env.pop(key, None)


def run(*args, body=None, allow=False, extra=None):
    cp = subprocess.run([binary, "thread", *args], cwd=a, env=dict(env, **(extra or {})), input=body, text=True,
                        capture_output=True, timeout=120)
    if not allow and cp.returncode:
        raise RuntimeError(str(args) + " " + cp.stderr[-1500:])
    return cp


def act(op, actor=None, **args):
    return json.loads(run("captain", "act", op, body=json.dumps(args), extra={"PA_CAPTAIN_ACTOR": actor} if actor else None).stdout)


def refused(op, needle, actor=None, **args):
    cp = run("captain", "act", op, body=json.dumps(args), allow=True, extra={"PA_CAPTAIN_ACTOR": actor} if actor else None)
    assert cp.returncode and needle in cp.stderr and "Traceback" not in cp.stderr, (op, cp.stderr)


def db():
    return sqlite3.connect(hub / "captains/projects.sqlite3")


def one(sql, *params):
    return db().execute(sql, params).fetchone()


def status():
    return json.loads(run("captain", "status").stdout)["projects"][0]


def member(project, mid):
    return next(m for m in project["crew"] if m["id"] == mid)


def wait(f, what, limit=90):
    for _ in range(int(limit / 0.2)):
        value = f()
        if value:
            return value
        time.sleep(0.2)
    raise RuntimeError("timeout: " + what)


def settled(jid):
    return wait(lambda: one("select status from jobs where id=?", jid)[0] not in ("queued", "running", "parked"), "job " + jid)


def git(*args, cwd=a):
    return subprocess.check_output(["git", "-C", str(cwd), *args], text=True).strip()


def captain_processes():
    listed = subprocess.run(["ps", "-wwEAo", "pid=,command="], capture_output=True, text=True).stdout
    return [line for line in listed.splitlines() if " thread captain " in line and str(hub) in line]


# The cron evaluator and the CI summary are pure functions; run them from the module source.
module = {"datetime": datetime}
exec((P(__file__).parent.parent / "src/captain_schedules.py").read_text(), module)
cron = module["next_cron_time"]


def local(*parts):
    return datetime.datetime(*parts).astimezone()


assert cron("*/15 * * * *", local(2026, 11, 23, 10, 7)) == local(2026, 11, 23, 10, 15)
assert cron("5,10 * * * *", local(2026, 11, 23, 10, 5)) == local(2026, 11, 23, 10, 10)
assert cron("0 9 * * 1-5", local(2026, 11, 21, 12, 0)) == local(2026, 11, 23, 9, 0)  # Saturday to Monday
assert cron("0 12 * * 7", local(2026, 11, 23, 0, 0)) == cron("0 12 * * 0", local(2026, 11, 23, 0, 0)) == local(2026, 11, 29, 12, 0)
assert cron("30 2 1 * *", local(2026, 11, 23, 0, 0)) == local(2026, 12, 1, 2, 30)
assert cron("0 0 29 2 *", local(2026, 3, 1)) == local(2028, 2, 29)
assert cron("0 8-10/2 * 1,6 *", local(2026, 11, 23)) == local(2027, 1, 1, 8, 0)
assert cron("0 0 13 * 5", local(2026, 11, 23)) == local(2026, 11, 27)  # Friday or the 13th, whichever first
for bad in ("61 * * * *", "* * *", "*/0 * * * *", "a * * * *", "5-2 * * * *", "0 0 30 2 *"):
    try:
        cron(bad, local(2026, 11, 23))
        raise AssertionError(bad)
    except ValueError:
        pass
checks = module["pr_checks"]
assert checks([{"status": "COMPLETED", "conclusion": "SUCCESS"}, {"status": "IN_PROGRESS"}]) == "pending"
assert checks([{"status": "COMPLETED", "conclusion": "FAILURE"}, {"status": "IN_PROGRESS"}]) == "fail"
assert checks([{"state": "SUCCESS"}, {"status": "COMPLETED", "conclusion": "SKIPPED"}]) == "pass"
print("Verified the five-field cron evaluator and the CI check summary.", flush=True)

pid = None
try:
    git("init", "-q")
    (a / "a.txt").write_text("one\n")
    git("add", "a.txt")
    git("-c", "user.name=Fixture", "-c", "user.email=test@localhost", "commit", "-qm", "base")
    base = git("rev-parse", "HEAD")
    pid = act("init", objective="Ship Alpha", provider="claude")["id"]
    brief = one("select id from jobs where purpose='brief'")[0]
    settled(brief)
    lead = status()["lead"]

    # Escalation decision card: the Captain asks, the user answers with one key, the Captain acts.
    options = [{"key": "1", "label": "SQLite", "reply_text": "Use SQLite for the store."},
               {"key": "2", "label": "Postgres", "reply_text": "Use Postgres for the store."}]
    refused("escalate", "A crew thread cannot run Captain actions", actor="crew:" + pid, project=pid, title="x", summary="y", options=options)
    refused("escalate", "1 to 6", actor=pid, project=pid, title="x", summary="y", options=options * 4)
    refused("escalate", "unique key", actor=pid, project=pid, title="x", summary="y", options=[options[0], options[0]])
    did = act("escalate", actor=pid, project=pid, title="Pick a store", summary="Two options fit.", recommendation="1", options=options)["id"]
    project = status()
    card = next(i for i in project["needs_you"] if i["kind"] == "decision")
    assert card["decision_id"] == did and card["title"] == "Pick a store" and card["summary"] == "Two options fit.", card
    assert card["recommendation"] == "1" and card["options"] == options, card
    assert [d["id"] for d in project["decisions"]] == [did], project["decisions"]
    published = json.loads((hub / "captains/status" / (pid + ".json")).read_text())
    assert any(i["kind"] == "decision" for i in published["needs_you"]), published["needs_you"]
    assert one("select kind from notifications where kind='decision'")
    refused("decision-resolve", "Use an action", actor=pid, project=pid, id=did, key="1")
    refused("decision-resolve", "Choose one of the keys", project=pid, id=did, key="9")
    resolved = act("decision-resolve", project=pid, id=did, key="1")
    assert resolved["status"] == "resolved" and resolved["chosen_key"] == "1", resolved
    assert one("select status,chosen_key,resolved_at!='' from decisions where id=?", did) == ("resolved", "1", 1)
    job = one("select member,purpose,body from jobs where id=?", resolved["job"])
    assert job[0] == lead and job[1] == "lead" and "Use SQLite for the store." in job[2], job
    assert act("decision-resolve", project=pid, id=did, key="2")["repeat"] is True
    settled(resolved["job"])
    assert one("select status from jobs where id=?", resolved["job"])[0] == "done"
    assert not [i for i in status()["needs_you"] if i["kind"] == "decision"]
    print("Verified escalate shows a decision card and decision-resolve queues the chosen reply as a lead turn.", flush=True)

    # Cron schedules: add, list, toggle, fire on the normal job queue, delete.
    refused("schedule-add", "only in a turn that answers a user message", actor=pid, project=pid, title="x", cron_expr="* * * * *", task="x")
    refused("schedule-add", "five fields", project=pid, title="Bad", cron_expr="every day", task="x")
    added = act("schedule-add", project=pid, title="Nightly", cron_expr="0 3 * * *", task="SCHEDULED-TASK check CI")
    sid = added["id"]
    nxt = datetime.datetime.fromisoformat(added["next_run_at"])
    assert nxt > datetime.datetime.now(datetime.timezone.utc) and nxt.astimezone().hour == 3, added
    listed = act("schedule-list", actor=pid, project=pid)["schedules"]
    assert [(s["id"], s["enabled"], s["cron_expr"]) for s in listed] == [(sid, True, "0 3 * * *")], listed
    assert act("schedule-toggle", project=pid, id=sid, enabled=False)["next_run_at"] == ""
    assert one("select enabled,next_run_at from schedules where id=?", sid) == (0, "")
    assert act("schedule-toggle", project=pid, id=sid, enabled=True)["next_run_at"] == added["next_run_at"]
    past = (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=1)).isoformat()
    with db() as c:
        c.execute("update schedules set next_run_at=? where id=?", (past, sid))
    fired = wait(lambda: one("select id from jobs where origin like ?", "schedule/" + sid + "/%"), "the scheduled job")[0]
    row = one("select member,purpose,body,request from jobs where id=?", fired)
    assert row[0] == lead and row[1] == "lead" and row[2] == "SCHEDULED-TASK check CI", row
    assert json.loads(row[3])[1]["source"] == "schedule", row
    settled(fired)
    assert one("select status from jobs where id=?", fired)[0] == "done", db().execute("select id,purpose,status,created,substr(result,1,80),origin from jobs order by created").fetchall()
    sched = status()["schedules"][0]
    assert sched["last_run_at"] and datetime.datetime.fromisoformat(sched["next_run_at"]) > datetime.datetime.now(datetime.timezone.utc), sched
    assert one("select count(*) from jobs where origin like ?", "schedule/" + sid + "/%")[0] == 1
    assert one("select title from notifications where kind='schedule'")[0] == "Schedule ran: Nightly"
    refused("schedule-delete", "Use an action", actor=pid, project=pid, id=sid)
    assert act("schedule-delete", project=pid, id=sid)["deleted"] is True
    assert act("schedule-list", project=pid)["schedules"] == []
    print("Verified schedule-add, list, toggle, delete, and a due schedule firing one lead job.", flush=True)

    # Budget: a spent cap is a Needs you item, holds digests, refuses automated delegation, and
    # extending it releases the held work. The user's own turns still run.
    assert act("budget", actor=pid, project=pid)["budget"] == {
        "budget_usd": None, "budget_tokens": None, "spent_usd": 0.0, "spent_tokens": 0, "exceeded": False}
    refused("budget", "Only you set", actor=pid, project=pid, budget_usd=100)
    act("budget", project=pid, budget_usd=0.5)
    with db() as c:
        c.execute("update jobs set cost_usd=1.0,input_tokens=10,output_tokens=5 where id=?", (brief,))
    project = status()
    assert project["budget"] == {"budget_usd": 0.5, "budget_tokens": None, "spent_usd": 1.0, "spent_tokens": 15, "exceeded": True}, project["budget"]
    item = next(i for i in project["needs_you"] if i["kind"] == "budget_exceeded")
    assert item["spent_usd"] == 1.0 and "$1.00 of $0.50" in item["detail"], item
    refused("delegate", "budget is spent", actor=pid, project=pid, provider="claude", task="More work", request_id="bud-1")
    digest = "00000000-0000-4000-8000-000000000d10"
    with db() as c:
        c.execute("insert into jobs(id,project,member,purpose,body,status,created) values(?,?,?,'digest','Review.','queued',?)",
                  (digest, pid, lead, time.time()))
    run("captain", "run-job", digest)
    run("captain", "reconcile")
    time.sleep(1)
    assert one("select status from jobs where id=?", digest)[0] == "queued"
    extended = act("budget", project=pid, extend_usd=1.0)["budget"]
    assert extended["budget_usd"] == 1.5 and not extended["exceeded"], extended
    settled(digest)
    assert not [i for i in status()["needs_you"] if i["kind"] == "budget_exceeded"]
    act("budget", project=pid, budget_tokens=10)
    assert status()["budget"]["exceeded"] is True
    user_turn = act("lead", project=pid, text="Keep going anyway")["job"]
    settled(user_turn)
    assert one("select status from jobs where id=?", user_turn)[0] == "done"
    assert act("budget", project=pid, clear=True)["budget"]["exceeded"] is False
    print("Verified budget caps: Needs you item, held digest, refused automated delegate, extend, and clear.", flush=True)

    # A stacked writer waits for its parent, then starts on the parent's HEAD with its handoff.
    hold.touch()
    parent = act("delegate", project=pid, provider="claude", task="PARENT-TASK lay the base", write=True, request_id="stack-p")["id"]
    child = act("delegate", project=pid, provider="claude", task="CHILD-TASK build on the base", write=True,
                depends_on=parent, request_id="stack-c")
    assert child["parked"] is True, child
    child_job, child = child["job"], child["id"]
    refused("delegate", "depends_on must name", project=pid, provider="claude", task="x", write=True, depends_on="nope", request_id="stack-x")
    refused("delegate", "Pass write true", project=pid, provider="claude", task="x", depends_on=parent, request_id="stack-y")
    project = status()
    row = member(project, child)
    assert row["activity_state"] == "waiting" and row["detail"].startswith("Waiting on "), row
    assert row["depends_on"] == parent and row["stack_position"] == "2/2", row
    assert member(project, parent)["stack_position"] == "1/2" and member(project, parent)["depends_on"] == ""
    assert one("select status,base_ref from jobs where id=?", child_job) == ("parked", None)
    assert not (hub / "captains/worktrees" / child).exists()
    hold.unlink()
    parent_job = one("select id from jobs where member=?", parent)[0]
    settled(parent_job)
    settled(child_job)
    ptree, ctree = hub / "captains/worktrees" / parent, hub / "captains/worktrees" / child
    parent_head = git("rev-parse", "HEAD", cwd=ptree)
    assert parent_head != base and git("rev-parse", "HEAD~1", cwd=ptree) == base
    assert one("select status,base_ref from jobs where id=?", child_job) == ("done", parent_head)
    assert git("rev-parse", "HEAD~1", cwd=ctree) == parent_head
    handoff = ".prime/captain/handoffs/" + parent[:8] + ".json"
    assert handoff in one("select body from jobs where id=?", child_job)[0]
    assert json.loads((a / handoff).read_text())["head_sha"] == parent_head
    assert "handoffs/" + parent[:8] + ".json" in status()["drive_files"]
    print("Verified a depends_on branch parks, then starts on its parent's HEAD with the parent's handoff.", flush=True)

    # Pre-turn checkpoints are refs only; rollback restores a commit and its uncommitted changes.
    refs = lambda tree, mid: git("for-each-ref", "--format=%(refname) %(objectname)", "refs/pa/checkpoints/" + mid + "/", cwd=tree).splitlines()
    assert refs(ptree, parent) == [f"refs/pa/checkpoints/{parent}/1 {base}"], refs(ptree, parent)
    assert refs(ctree, child) == [f"refs/pa/checkpoints/{child}/1 {parent_head}"], refs(ctree, child)
    assert git("status", "--porcelain", cwd=ptree) == ""
    exclude = P(git("rev-parse", "--git-common-dir", cwd=a)) / "info/exclude"
    if not exclude.is_absolute():
        exclude = a / exclude
    exclude.parent.mkdir(parents=True, exist_ok=True)
    with exclude.open("a") as f:
        f.write("*.log\n")
    (ptree / "a.txt").write_text("two\n")
    (ptree / "new_file.py").write_text("print('untracked')\n")
    (ptree / "keep.log").write_text("ignored\n")
    reply = act("reply", project=pid, member=parent, text="REPLY-TASK polish", request_id="r-1")["job"]
    settled(reply)
    rows = refs(ptree, parent)
    snap = rows[1].split()[1]
    assert len(rows) == 2 and git("rev-parse", snap + "^1", cwd=ptree) == parent_head, rows
    assert git("show", snap + ":new_file.py", cwd=ptree) == "print('untracked')"
    assert git("show", snap + ":a.txt", cwd=ptree) == "two"
    assert "keep.log" not in git("ls-tree", "--name-only", snap, cwd=ptree).split()
    assert (ptree / "a.txt").read_text() == "two\n"
    assert git("status", "--porcelain", cwd=ptree) == "M a.txt\n?? new_file.py", git("status", "--porcelain", cwd=ptree)
    assert git("stash", "list", cwd=ptree) == "", "the checkpoint must not push a stash entry"
    refused("checkpoint-rollback", "Use an action", actor=pid, project=pid, member=parent)
    first = act("checkpoint-rollback", project=pid, member=parent, ref="1")
    assert first["sha"] == base and first["restored_changes"] is False, first
    assert (ptree / "a.txt").read_text() == "one\n" and git("status", "--porcelain", cwd=ptree) == ""
    assert not (ptree / "new_file.py").exists() and (ptree / "keep.log").read_text() == "ignored\n"
    (ptree / "a.txt").unlink()
    (ptree / "scratch.py").write_text("later\n")
    latest = act("checkpoint-rollback", project=pid, member=parent)
    assert latest["ref"].endswith("/2") and latest["sha"] == parent_head and latest["restored_changes"] is True, latest
    assert (ptree / "a.txt").read_text() == "two\n"
    assert (ptree / "new_file.py").read_text() == "print('untracked')\n", "rollback must restore untracked files"
    assert not (ptree / "scratch.py").exists() and (ptree / "keep.log").read_text() == "ignored\n"
    assert git("status", "--porcelain", cwd=ptree) == "M a.txt\n?? new_file.py"
    assert git("rev-parse", "HEAD", cwd=ptree) == parent_head
    refused("checkpoint-rollback", "No checkpoint 9", project=pid, member=parent, ref="9")
    print("Verified checkpoints snapshot tracked and untracked changes and checkpoint-rollback restores both.", flush=True)

    # A writer delegated off a dirty project root starts from a snapshot of its files.
    root_head = git("rev-parse", "HEAD")
    root_status = git("status", "--porcelain")
    (a / "a.txt").write_text("root edit\n")
    (a / "root_new.py").write_text("x = 1\n")
    dirty_status = git("status", "--porcelain")
    snapped = act("delegate", project=pid, provider="claude", task="SNAP-TASK use the root files", write=True, request_id="snap-1")
    settled(snapped["job"])
    stree = hub / "captains/worktrees" / snapped["id"]
    base_snap = git("rev-parse", "refs/pa/bases/" + snapped["id"])
    assert base_snap != root_head and git("rev-parse", base_snap + "^1") == root_head
    assert ".prime" not in git("ls-tree", "--name-only", base_snap).split(), "Captain files stay out of the base"
    assert one("select base_ref from jobs where id=?", snapped["job"])[0] == base_snap
    assert git("rev-parse", "HEAD~1", cwd=stree) == base_snap
    assert (stree / "a.txt").read_text() == "root edit\n" and (stree / "root_new.py").read_text() == "x = 1\n"
    assert git("rev-parse", "HEAD") == root_head and git("status", "--porcelain") == dirty_status, "the root must stay untouched"
    (a / "a.txt").write_text("one\n")
    (a / "root_new.py").unlink()
    assert git("status", "--porcelain") == root_status
    print("Verified a writer forks from refs/pa/bases/<member> holding the root's uncommitted and untracked files.", flush=True)

    # Review findings: keyed by file, normalized snippet, and category; triage survives re-review.
    f1 = {"file": "src/a.rs", "snippet": "let x =  1;\n", "category": "bug", "severity": "high", "message": "x is unused", "line_hint": 3}
    f2 = {"file": "src/b.rs", "snippet": "fn b() {}", "category": "style", "severity": "low", "message": "Name b better"}
    out = act("review-findings", actor=pid, project=pid, member=parent, findings=[f1, f2])
    assert out["ok"] and out["upserted"] == 2 and [f["status"] for f in out["findings"]] == ["open", "open"], out
    ids = {f["file"]: f["id"] for f in out["findings"]}
    refused("review-triage", "Use an action", actor=pid, project=pid, id=ids["src/a.rs"], status="resolved")
    refused("review-triage", "open, resolved, or irrelevant", project=pid, id=ids["src/a.rs"], status="done")
    act("review-triage", project=pid, id=ids["src/a.rs"], status="resolved")
    act("review-triage", project=pid, id=ids["src/b.rs"], status="irrelevant")
    moved = dict(f1, snippet="    let x = 1;", line_hint=9, message="x is still unused")
    f3 = {"file": "src/a.rs", "snippet_hash": "abc123", "category": "bug", "severity": "medium", "message": "New issue"}
    out = act("review-findings", actor=pid, project=pid, member=parent, findings=[moved, f2, f3])
    found = {f["id"]: f for f in out["findings"]}
    assert len(found) == 3 and found[ids["src/a.rs"]]["status"] == "resolved", out
    assert found[ids["src/a.rs"]]["line_hint"] == 9 and found[ids["src/a.rs"]]["message"] == "x is still unused"
    assert found[ids["src/b.rs"]]["status"] == "irrelevant"
    opened = act("review-findings", project=pid, status="open")["findings"]
    assert [f["snippet_hash"] for f in opened] == ["abc123"], opened
    assert act("review-findings", project=pid, member=child)["findings"] == []
    assert member(status(), parent)["open_findings_count"] == 1
    print("Verified review findings upsert by snippet, keep triage across re-review, and filter.", flush=True)

    # PR watch: an explicit URL, gh auto-detection in the worktree, and a polled pill in status.
    out = act("pr-watch", actor=pid, project=pid, member=parent, url="https://github.com/o/r/pull/7", number=7)
    assert out["pr_url"].endswith("/pull/7") and out["pr_number"] == 7, out
    assert one("select pr_url,pr_number from crew where id=?", parent) == ("https://github.com/o/r/pull/7", 7)
    out = act("pr-watch", project=pid, member=child)
    assert (out["pr_url"], out["pr_number"]) == ("https://github.com/o/r/pull/8", 8), out
    cp = run("captain", "act", "pr-watch", body=json.dumps({"project": pid, "member": child}), allow=True,
             extra={"PA_CAPTAIN_DISABLE_GH": "1"})
    assert cp.returncode and "gh is not available" in cp.stderr, cp.stderr
    pill = wait(lambda: member(status(), parent)["pr_status"], "the polled PR status")
    assert pill["state"] == "MERGED" and pill["checks"] == "pass" and pill["review_decision"] == "APPROVED", pill
    wait(lambda: one("select 1 from reports where id like ?", "pr/" + parent + "/%"), "the PR merge report")
    print("Verified pr-watch stores the PR, detects it with gh, and polls a cached PR pill.", flush=True)
finally:
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as c:
            c.execute("delete from watches")
            c.execute("delete from schedules")
        if pid:
            run("captain", "stop", allow=True)
    hold.unlink(missing_ok=True)
wait(lambda: not captain_processes(), "Captain workers to exit", 30)
print("ok")
