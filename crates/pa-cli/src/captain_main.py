HELP = (
    "pa thread captain status              Show every project as JSON\n"
    "pa thread captain act OP              Read action arguments as JSON from standard input\n"
    "pa thread captain stop|resume|delete [PROJECT]\n"
    "                                      Stop, resume, or delete this project's Captain\n"
    "Open the Captain panel in pa with F6 or /captain.\n"
    "Actions: init, promote, edit, delegate, adopt, release, reply, answer, grant, lead,\n"
    "open, transcript, choices, review, factory, consensus, stop, resume, delete,\n"
    "factory-validate, factory-show, factory-approve, factory-delete, factory-design\n"
    "Project factories live in <project>/.prime/machines/<name>/ and run only after the user approves them."
)


def captain_main(args):
    if args in ([], ["--help"], ["help"]):
        print(HELP)
        return
    if args and args[0] in ("stop", "resume", "delete") and len(args) <= 2:
        result = action(args[0], project=args[1] if len(args) == 2 else None)
        print(json.dumps(result, ensure_ascii=False))
        return
    if args == ["status"]:
        result = snapshot()
        if os.getenv("PA_CAPTAIN_ACTOR"):
            own = os.environ["PA_CAPTAIN_ACTOR"].removeprefix("crew:")
            result["projects"] = [p for p in result["projects"] if p["id"] == own]
        print(json.dumps(result, ensure_ascii=False))
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
        actor = os.getenv("PA_CAPTAIN_ACTOR")
        if actor and actor.startswith("crew:"):
            # Crew threads and factory hosts may read a project factory, nothing more.
            if args[1] not in ("factory-validate", "factory-show") or data.get("project") != actor[5:]:
                raise ValueError("A crew thread cannot run Captain actions.")
        elif actor and (
            args[1]
            not in (
                "delegate", "adopt", "edit", "release", "reply", "answer", "grant",
                "factory-validate", "factory-show", "factory-approve", "factory-delete",
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
