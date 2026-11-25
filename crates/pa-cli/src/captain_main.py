HELP = (
    "pa thread captain status              Show every project as JSON\n"
    "pa thread captain act OP              Read action arguments as JSON from standard input\n"
    "pa thread captain stop|resume|archive|unarchive|delete [PROJECT]\n"
    "                                      Stop, resume, archive, unarchive, or delete this project's Captain\n"
    "pa thread captain reconcile           Settle interrupted work once (safe to repeat)\n"
    "Open the Captain panel in pa with F6 or /captain.\n"
    "Actions: init, promote, edit, edit-history, edit-restore, crew-model, delegate, adopt, release,\n"
    "reply, answer, grant, grant-approve, grant-decline, archive, unarchive, worktree-recreate, lead, steer,\n"
    "open, transcript, choices, review, factory, consensus, stop, resume, delete,\n"
    "archive-project, unarchive-project (archive and unarchive with no member act on the Captain),\n"
    "wake-policy, notifications, notifications-read, mark-read,\n"
    "factory-validate, factory-show, factory-approve, factory-preapprove, factory-delete, factory-design,\n"
    "factory-set-model, factory-clear-model-override, factory-set-default-model,\n"
    "factory-compose, factory-add-stage, factory-remove-stage,\n"
    "factory-modules, factory-add-module, factory-update-module, factory-use-specialist,\n"
    "factory-update-specialist, specialist-list, specialist-show, specialist-write, specialist-delete,\n"
    "escalate, decision-resolve, budget, schedule-add, schedule-list, schedule-toggle, schedule-delete,\n"
    "pr-watch, review-findings, review-triage, checkpoint-rollback\n"
    "  init {brief,root?,create_dir?,...}           Start a Captain in root (an absolute folder;\n"
    "                                               default this folder); create_dir makes a missing folder\n"
    "  escalate {title,summary,recommendation?,options:[{key,label,reply_text}],member?}  A decision card\n"
    "  decision-resolve {id,key?|reply_text?}       Answer a card; the Captain acts on it at once (you only)\n"
    "  budget {budget_usd?,budget_tokens?,extend_usd?,extend_tokens?,clear?}  Read or set the spend cap\n"
    "  schedule-add {title,cron_expr,task}           Queue a Captain turn on a 5-field cron (local time)\n"
    "  schedule-toggle {id,enabled}, schedule-delete {id} (you only), schedule-list\n"
    "  pr-watch {member,url?,number?}               Watch a crew branch's pull request and CI with gh\n"
    "  review-findings {member?,status?,findings?}  Record findings by file, snippet, and category; list them\n"
    "  review-triage {id,status}                    Mark a finding open, resolved, or irrelevant (you only)\n"
    "  checkpoint-rollback {member,ref?}            Reset a worktree to a pre-turn checkpoint (you only)\n"
    "delegate also takes depends_on=<member> to stack a writer branch on another writer's finished HEAD.\n"
    "Project factories live in <project>/.prime/machines/<name>/ and run only after the user approves them."
)


def captain_main(args):
    if args in ([], ["--help"], ["help"]):
        print(HELP)
        return
    if args and args[0] in ("stop", "resume", "archive", "unarchive", "delete") and len(args) <= 2:
        if actor_kind() != "user":
            raise ValueError("Only you can stop, resume, archive, or delete a Captain.")
        result = action(args[0], project=args[1] if len(args) == 2 else None)
        print(json.dumps(result, ensure_ascii=False))
        return
    if args == ["status"]:
        result = snapshot()
        actor = effective_actor()
        if actor:
            own = actor.removeprefix("crew:")
            result["projects"] = [p for p in result["projects"] if p["id"] == own]
        print(json.dumps(result, ensure_ascii=False))
        return
    if args == ["reconcile"]:
        # Run at daemon start: a missing store means no Captain exists, so create nothing.
        if STORE.is_file():
            reconcile()
            with database() as db:
                watching = bool(db.execute("SELECT 1 FROM watches LIMIT 1").fetchone())
            if watching:
                start_watch()
        print(json.dumps({"ok": True, "store": STORE.is_file()}))
        return
    if args == ["watch"]:
        watch()
        return
    if len(args) == 2 and args[0] == "run-job":
        work(args[1])
        return
    if len(args) == 2 and args[0] == "act":
        data = json.load(sys.stdin)
        if not isinstance(data, dict):
            raise ValueError("Expected a JSON object.")
        actor = effective_actor()
        if actor and actor.startswith("crew:"):
            # Crew threads and factory hosts may read a project factory, nothing more.
            if args[1] not in (
                "factory-validate", "factory-show", "factory-modules", "specialist-list", "specialist-show",
            ) or data.get("project") != actor[5:]:
                raise ValueError("A crew thread cannot run Captain actions.")
        elif actor and (
            args[1]
            not in (
                "delegate", "adopt", "edit", "edit-history", "release", "reply", "answer", "grant",
                "archive", "unarchive",
                "wake-policy", "notifications", "notifications-read", "mark-read",
                "factory-validate", "factory-show", "factory-approve", "factory-preapprove",
                "factory-delete", "factory-design",
                "factory-set-model", "factory-clear-model-override",
                "factory-compose", "factory-add-stage", "factory-remove-stage",
                *MODULE_OPS, "specialist-list", "specialist-show", "specialist-write",
                "factory-list", "escalate", "schedule-add", "schedule-list", "schedule-toggle",
                "budget", "pr-watch", "review-findings",
            )
            or data.get("project") != actor
        ):
            raise ValueError("Use an action in this Captain's project.")
        print(json.dumps(action(args[1], **data), ensure_ascii=False))
        return
    raise ValueError("See pa thread captain help.")


try:
    captain_main(sys.argv[2:])
except (
    ValueError,
    OSError,
    sqlite3.Error,
    KeyError,
    TypeError,
    subprocess.SubprocessError,
) as error:
    sys.exit(f"Captain: {error}")
