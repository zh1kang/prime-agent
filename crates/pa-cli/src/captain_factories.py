"""Project factories: machines a Captain writes under <project root>/.prime/machines/<name>/.

The Captain writes these files itself, so the user approves one exact content version.

States of one project factory:
    missing   no <name>/machine.toml in the project's machines folder.
    invalid   the files exist but do not load: `pa factory validate` fails, a symlink leaves the
              folder, the name does not match the folder, or a library machine has the name.
    draft     valid, and the user never approved it.
    approved  valid, and its digest equals the approved digest.
    changed   valid, and its digest differs from the approved digest.

Only `approved` runs. Any edit after approval changes the digest, so the factory is a draft
again until the user approves the new version. A project factory always writes code: its
captain.toml is ignored, because the Captain could write `access = "read"` itself. Grants by
name never apply to it; the approval of a digest is its permission. A run copies the approved
files into its control folder and checks the copy's digest, so a later edit cannot change a
run that started.
"""

import hashlib
import shutil
import tomllib

FACTORY_NAME = re.compile(r"[a-z0-9][a-z0-9-]{0,63}")
DESIGN_REQUEST = "Design a factory for this project."
# Bytecode that python nodes write while they run; it is not part of the approved version.
FACTORY_IGNORED = ("__pycache__",)


def factories_dir(p):
    return Path(p["root"]) / ".prime" / "machines"


def project_machines_dir(p):
    """<project>/.prime/machines for a change; refused when .prime or machines is a symlink or
    leaves the project, so a delete or a write cannot reach outside it."""
    root = Path(p["root"]).resolve()
    directory = root / ".prime" / "machines"
    for part in (root / ".prime", directory):
        if part.is_symlink() or (part.exists() and not part.resolve().is_relative_to(root)):
            raise ValueError(f"{part} is a symlink or leaves the project folder. Remove it yourself.")
    return directory


def checked_factory_name(name):
    if not isinstance(name, str) or not FACTORY_NAME.fullmatch(name):
        raise ValueError(
            "Name a project factory with lowercase letters, digits, and dashes (at most 64)."
        )
    return name


def factory_files(directory):
    """(relative path, real path) for every file of a machine directory, sorted.

    A symlink whose target leaves the directory is an error, as in the machine loader.
    """
    root = Path(directory).resolve(strict=True)
    files = []

    def walk(current, rel, ancestors):
        for child in sorted(current.iterdir(), key=lambda c: c.name):
            if child.name in FACTORY_IGNORED:
                continue
            path = f"{rel}/{child.name}" if rel else child.name
            target = child.resolve()
            if target != root and root not in target.parents:
                raise ValueError(f"{path}: the symlink target {target} is outside {root}")
            if child.is_dir():
                if target in ancestors:
                    raise ValueError(f"{path}: the symlink makes a loop in {root}")
                walk(child, path, ancestors | {target})
            elif child.is_file():
                files.append((path, target))
            else:
                raise ValueError(f"{path}: not a regular file or folder")

    walk(root, "", {root})
    return files


def factory_digest(directory):
    """SHA-256 over the sorted relative paths and the bytes of every file."""
    digest = hashlib.sha256()
    for path, real in factory_files(directory):
        data = real.read_bytes()
        digest.update(path.encode() + b"\0" + str(len(data)).encode() + b"\0" + data)
    return digest.hexdigest()


def factory_approvals(p):
    with database() as db:
        return {
            r["name"]: r["digest"]
            for r in db.execute(
                "SELECT name,digest FROM factory_approvals WHERE project=?", (p["id"],)
            )
        }


def factory_names(p, approvals=None):
    """Folders with a machine.toml, and approved names whose folder is gone."""
    root = factories_dir(p)
    names = set(approvals or ())
    if root.is_dir():
        names |= {
            d.name
            for d in root.iterdir()
            if FACTORY_NAME.fullmatch(d.name) and (d / "machine.toml").exists()
        }
    return sorted(names)


def snapshot_files(source, target):
    """Copy the files of a machine directory into `target` (no `__pycache__`, no symlinks)."""
    for path, real in factory_files(source):
        destination = target / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(real, destination)


def toml_strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from toml_strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from toml_strings(item)


def factory_status(p, name, approved=None, library=None, graph=False, overlay=None):
    """One project factory's state, digest, and errors. Its own content never raises here.

    The folder is copied once into a temporary snapshot, and the digest, the validation, and
    the compiled graph all come from that snapshot, so they describe one version. The model
    overlay is policy, not content: the graph and roles show it, the digest never covers it.
    """
    name = checked_factory_name(name)
    directory = factories_dir(p) / name
    row = {
        "name": name,
        "path": str(directory),
        "file": str(directory / "machine.toml"),
        "digest": "",
        "approved": bool(approved),
        "errors": [],
    }
    if not (directory / "machine.toml").is_file():
        row["state"] = "missing"
        return row
    errors = []
    with tempfile.TemporaryDirectory(prefix="pa-factory-") as temporary:
        snapshot = Path(temporary) / name
        try:
            snapshot_files(directory, snapshot)
            row["digest"] = factory_digest(snapshot)
        except (OSError, ValueError) as error:
            errors.append(str(error))
        if library is None:
            library = installed_machines()
        if name in library:
            errors.append(f"A library machine named {name} exists. Rename this project factory.")
        manifest = None
        if not errors:
            try:
                manifest = tomllib.loads((snapshot / "machine.toml").read_text())
            except (OSError, ValueError):
                manifest = None  # validate reports the parse error
        if isinstance(manifest, dict):
            declared = manifest.get("name")
            if declared is not None and declared != name:
                errors.append(f"machine.toml: name is {declared!r}; it must be the folder name {name}.")
            for text in toml_strings(manifest):
                if "__pycache__" in re.split(r"[/\\:]", text):
                    errors.append(f"machine.toml: {text!r} names a path under __pycache__, which approval does not cover.")
        if not errors:
            check = subprocess.run(
                [PA, "factory", "validate", str(snapshot)],
                capture_output=True,
                text=True,
                timeout=60,
                stdin=subprocess.DEVNULL,
            )
            if check.returncode:
                lines = (check.stderr or check.stdout).replace(str(snapshot), str(directory)).strip().splitlines()
                errors += [line.removeprefix("Error: ") for line in lines if line.strip()] or [
                    "Validation failed."
                ]
        if not errors:
            row["modules"], row["specialists"], row["drift"] = factory_lock_rows(p, snapshot)
        if graph and not errors and overlay and any(overlay.values()):
            view = Path(temporary) / "view" / name
            snapshot_files(snapshot, view)
            try:
                apply_model_overlay(view, overlay)
                snapshot = view
                manifest = tomllib.loads((view / "machine.toml").read_text())
            except (OSError, ValueError) as error:
                row["overlay_error"] = str(error)
        if graph and not errors:
            row["graph"], row["graph_error"] = factory_graph(snapshot, manifest or {})
            # The model of steps that name no model or role (the run's host), and the factory's
            # own default role, which the review page edits for all steps that use it.
            inherit_model, inherit_thinking = factory_default_model(p)
            row["inherit_model"] = {"model": inherit_model, "thinking": inherit_thinking}
            roles = (manifest or {}).get("roles") if isinstance(manifest, dict) else None
            default = roles.get("default") if isinstance(roles, dict) else None
            if isinstance(default, dict) and isinstance(default.get("model"), str):
                row["default_role"] = {
                    "model": default["model"],
                    "thinking": str(default.get("thinking") or ""),
                }
            desc, params, details = factory_metadata(snapshot, manifest or {})
            row["description"] = desc
            row["params"] = params
            row["state_details"] = details
    row["errors"] = errors
    if errors:
        row["state"] = "invalid"
    elif not approved:
        row["state"] = "draft"
    else:
        row["state"] = "approved" if approved == row["digest"] else "changed"
    return row


def project_factories(p):
    """Every project factory of `p` with its state, for the snapshot and the Captain."""
    approvals = factory_approvals(p)
    names = factory_names(p, approvals)
    library = installed_machines() if names else {}
    rows = [factory_status(p, n, approvals.get(n), library) for n in names]
    for row in rows:
        row["model_overrides"] = get_factory_model_overlay(p, row["name"])
    return rows


def factory_placeholder(spec):
    """A value that type-checks for a required param, so a draft compiles for its diagram."""
    kind = str(spec.get("type", "text")) if isinstance(spec, dict) else "text"
    if kind in ("text", "file"):
        return None
    if kind in ("int", "float"):
        return "0"
    if kind == "bool":
        return "false"
    if kind.startswith("list["):
        return "[]"
    if kind.startswith("enum("):
        return json.dumps(kind[5:-1].split(",")[0].strip())
    return "null"


NODE_FILE_CAP = 65536


def _safe_read_inside(root, rel_path):
    """Read a relative file strictly inside `root`, capped at NODE_FILE_CAP."""
    root = Path(root).resolve()
    target = (root / rel_path).resolve()
    if target != root and root not in target.parents:
        return None
    if not target.is_file():
        return None
    raw = target.read_bytes()
    truncated = len(raw) > NODE_FILE_CAP
    text = raw[:NODE_FILE_CAP].decode("utf-8", errors="replace")
    return {"path": str(rel_path), "content": text, "truncated": truncated}


def factory_metadata(snapshot, manifest):
    """Description, params, and per-state details + source files for interactive inspection."""
    if not isinstance(manifest, dict):
        return "", [], {}
    description = str(manifest.get("description") or "")
    params = []
    for pname, spec in (manifest.get("params") or {}).items():
        if not isinstance(spec, dict):
            continue
        entry = {
            "name": str(pname),
            "type": str(spec.get("type", "text")),
            "required": "default" not in spec,
            "description": str(spec.get("description") or ""),
        }
        if "default" in spec:
            entry["default"] = spec["default"]
        params.append(entry)
    roles = manifest.get("roles") if isinstance(manifest.get("roles"), dict) else {}
    context_file = _safe_read_inside(snapshot, "prompts/_context.md")
    state_details = {}
    for sid, st in (manifest.get("states") or {}).items():
        if not isinstance(st, dict):
            continue
        is_python = st.get("kind") == "python"
        agent = st.get("agent") if isinstance(st.get("agent"), dict) else {}
        role = agent.get("role") if not is_python else None
        role_cfg = roles.get(role) if isinstance(role, str) and isinstance(roles.get(role), dict) else {}
        model = (agent.get("model") or role_cfg.get("model") or "") if not is_python else ""
        thinking = (agent.get("thinking") or role_cfg.get("thinking") or "") if not is_python else ""
        files = []
        if is_python:
            run_spec = str(st.get("run") or "")
            py_rel = run_spec.split(":", 1)[0]
            if py_rel:
                loaded = _safe_read_inside(snapshot, py_rel)
                if loaded:
                    files.append({"label": f"python ({run_spec})", **loaded})
        else:
            prompt_rel = agent.get("prompt")
            if isinstance(prompt_rel, str) and prompt_rel:
                loaded = _safe_read_inside(snapshot, prompt_rel)
                if loaded:
                    files.append({"label": f"prompt ({prompt_rel})", **loaded})
            rubric_rel = agent.get("rubric")
            if isinstance(rubric_rel, str) and rubric_rel:
                loaded = _safe_read_inside(snapshot, rubric_rel)
                if loaded:
                    files.append({"label": f"rubric ({rubric_rel})", **loaded})
            if context_file:
                files.append({"label": "shared context (prompts/_context.md)", **context_file})
        state_details[str(sid)] = {
            "id": str(sid),
            "kind": "python" if is_python else "agent",
            "entry": bool(st.get("entry")),
            "role": str(role) if isinstance(role, str) else "",
            "model": str(model),
            "thinking": str(thinking),
            "run": str(st.get("run") or "") if is_python else "",
            "budget": str(st.get("budget") or ""),
            "max_entries": st.get("max_entries"),
            "retries": st.get("retries"),
            "inputs": st.get("inputs") if isinstance(st.get("inputs"), dict) else {},
            "outputs": st.get("outputs") if isinstance(st.get("outputs"), dict) else {},
            "next": st.get("next"),
            "route": st.get("route") if isinstance(st.get("route"), dict) else None,
            "join": st.get("join") if isinstance(st.get("join"), list) else None,
            "files": files,
        }
    return description, params, state_details


def factory_graph(snapshot, manifest):
    """(compiled spec, error) for a valid snapshot, from `pa factory compile`."""
    params = manifest.get("params", {}) if isinstance(manifest, dict) else {}
    args = []
    for name, spec in params.items():
        if isinstance(spec, dict) and "default" in spec:
            continue
        value = factory_placeholder(spec)
        args += ["--param", f"{name}={'{' + name + '}' if value is None else value}"]
    run = subprocess.run(
        [PA, "factory", "compile", str(snapshot), *args],
        capture_output=True,
        text=True,
        timeout=60,
        stdin=subprocess.DEVNULL,
    )
    if run.returncode:
        return None, (run.stderr or run.stdout).strip().removeprefix("Error: ")[-1600:]
    try:
        return json.loads(run.stdout), None
    except ValueError as error:
        return None, str(error)


def factory_live(p, name):
    """Whether a queued or running Captain job runs this project factory."""
    with database() as db:
        bodies = db.execute(
            "SELECT body FROM jobs WHERE project=? AND purpose='machine' AND status IN ('queued','running')",
            (p["id"],),
        ).fetchall()
    for (body,) in bodies:
        try:
            spec = json.loads(body)
        except ValueError:
            continue
        if spec.get("source") == "project" and spec.get("name") == name:
            return True
    return False


def factory_refusal(row):
    """The NEEDS YOU message for a delegation of a factory that is not approved."""
    name = row["name"]
    if row["state"] == "invalid":
        return (
            f"{name} is invalid: {row['errors'][0]} Fix it, run factory-validate, "
            "then ask the user to approve it."
        )
    if row["state"] == "changed":
        return (
            f"NEEDS YOU: {name} changed since the user approved it. The user must review and "
            "approve this version in the Captain panel (F6, then F) before it runs."
        )
    return (
        f"NEEDS YOU: {name} is a draft. The user must approve it in the Captain panel "
        "(F6, then F) before it runs."
    )


def checked_project_factory(p, name):
    """The catalog row of an approved project factory, or the refusal for its state."""
    approvals = factory_approvals(p)
    row = factory_status(p, name, approvals.get(name))
    if row["state"] == "missing":
        return None
    if row["state"] != "approved":
        raise ValueError(factory_refusal(row))
    return {
        "name": name,
        "source": "project",
        "path": row["file"],
        "dir": row["path"],
        "digest": row["digest"],
    }


def copy_approved_factory(p, spec, control):
    """Copy the approved files into the run's control folder and return the copy.

    The copy must have the digest the dispatch checked, and that digest must still be the
    approved one. Then the run reads only files the user approved. The model overlay of the
    moment the run starts is applied to the copy; later overlay changes reach only later runs.
    """
    target = control / "machine" / spec["name"]
    if target.exists():
        shutil.rmtree(target)
    target.parent.mkdir(parents=True, exist_ok=True)
    stored = snapshot_dir(spec["digest"])
    source = stored if verify_snapshot(stored, spec["digest"]) else Path(spec["dir"])
    snapshot_files(source, target)
    copied = factory_digest(target)
    approved = factory_approvals(p).get(spec["name"])
    if copied != spec["digest"] or approved != spec["digest"]:
        raise ValueError(
            f"NEEDS YOU: {spec['name']} changed after it was approved. "
            "Review and approve it again in the Captain panel."
        )
    apply_model_overlay(target, project_overlay(p["id"], spec["name"]))
    return target


def fsm_edit(p, subop, payload):
    """One `pa thread fsm-edit` step on a project factory."""
    run = subprocess.run(
        [PA, "thread", "fsm-edit", subop],
        cwd=p["root"],
        env=thread_env(),
        input=json.dumps(payload),
        text=True,
        capture_output=True,
        timeout=60,
    )
    if run.returncode:
        raise ValueError((run.stderr or run.stdout or f"fsm-edit {subop} failed").strip()[-1200:])
    return json.loads(run.stdout)


def set_default_role(p, name, args):
    """factory-set-default-model: the user sets the factory's default role, which every step that
    uses it runs on. Like any edit, the factory needs approval again. A live run keeps its own copy."""
    _, model = checked_model("prime", args.get("model"))
    if not model:
        raise ValueError("Choose a model for the factory default.")
    thinking = checked_thinking(args.get("thinking"))
    draft = fsm_edit(p, "draft", {"name": name, "project": p["root"]})["draft"]
    try:
        fsm_edit(p, "set-role-model", {"draft": draft, "role": "default", "model": model, "thinking": thinking})
        fsm_edit(p, "save", {"draft": draft})
    except BaseException:
        try:
            fsm_edit(p, "discard", {"draft": draft})
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
        raise


# Edits a Captain may make in a turn that answers a user message, as well as the user.
LEAD_EDIT_OPS = ("factory-set-model", "factory-clear-model-override", "factory-preapprove")
LOCKED_OPS = ("factory-approve", "factory-set-model", "factory-set-default-model", "factory-clear-model-override")


def lead_or_user(p, op):
    """The edit history actor of an op only the user or a lead turn may run: user or captain-lead.
    The turn comes from the job queue, so a digest, brief, design, or crew turn is refused."""
    actor = actor_kind()
    if actor == "user":
        return "user"
    if actor == "captain" and captain_turn(p) == "lead":
        return "captain-lead"
    raise ValueError(
        f"Only the user or a Captain turn that answers a user message runs {op}. "
        "Ask the user with NEEDS YOU: first."
    )


def lead_preapprovals(p):
    with database() as db:
        row = db.execute("SELECT lead_approve_factories FROM projects WHERE id=?", (p["id"],)).fetchone()
    try:
        value = json.loads(row[0] if row else "{}")
    except (ValueError, TypeError):
        value = {}
    return value if isinstance(value, dict) else {}


def lead_preapproved(p, name):
    approvals = lead_preapprovals(p)
    return approvals.get(name) is True or approvals.get("*") is True


def set_preapproval(p, name, allow, actor):
    """Standing permission for lead turns to approve their own edits of `name` (or "*")."""
    if name != "*":
        name = checked_factory_name(name)
    with database(True) as db:
        row = db.execute("SELECT lead_approve_factories FROM projects WHERE id=?", (p["id"],)).fetchone()
        try:
            approvals = json.loads(row[0] if row else "{}")
        except (ValueError, TypeError):
            approvals = {}
        approvals = approvals if isinstance(approvals, dict) else {}
        old = json.dumps(approvals, sort_keys=True)
        if allow:
            approvals[name] = True
        else:
            approvals.pop(name, None)
        text = json.dumps(approvals, sort_keys=True)
        db.execute("UPDATE projects SET lead_approve_factories=? WHERE id=?", (text, p["id"]))
        record_edit(db, p["id"], "factory-preapprove " + name, old, text, actor, old_value=old)
    return {"ok": True, "name": name, "preapproved": bool(allow), "by": actor}


def factory_manifest(p, name):
    directory = factories_dir(p) / name
    try:
        manifest = tomllib.loads((directory / "machine.toml").read_text())
    except (OSError, ValueError) as error:
        raise ValueError(f"{name} has no readable machine.toml: {error}")
    return directory, manifest


def model_targets(manifest, args):
    """(roles, states) the model choice applies to: all, one role, one state, or the default role."""
    roles = manifest.get("roles") if isinstance(manifest.get("roles"), dict) else {}
    states = manifest.get("states") if isinstance(manifest.get("states"), dict) else {}
    agents = [sid for sid, st in states.items() if isinstance(st, dict) and st.get("kind") != "python"]
    if args.get("all") is True:
        return sorted(roles), sorted(agents)
    if args.get("state"):
        state = str(args["state"])
        if state not in agents:
            raise ValueError(f"{state} is not an agent state of this factory.")
        return [], [state]
    role = str(args.get("role") or "default")
    if role != "default" and role not in roles:
        raise ValueError(f"This factory has no role named {role}.")
    return [role], []


def checked_overlay(directory, name, overlay):
    """Refuse an overlay that does not apply to the factory's machine.toml."""
    with tempfile.TemporaryDirectory(prefix="pa-overlay-") as temporary:
        view = Path(temporary) / name
        view.mkdir()
        shutil.copyfile(directory / "machine.toml", view / "machine.toml")
        apply_model_overlay(view, overlay)


def sync_specialist_models(p, directory, roles, states, model, thinking, actor):
    """Write the chosen model into the frontmatter of the specialists the targets use."""
    lock = read_lock(directory)
    touched = []
    for sid, entry in sorted((lock.get("specialists") or {}).items()):
        if not (set(entry.get("states") or []) & set(states) or sid in roles):
            continue
        path = specialist_path(p, sid)
        if not path.is_file():
            continue
        text = path.read_text()
        head, sep, body = text.partition("\n---\n")
        if not head.startswith("---\n") or not sep:
            continue
        lines = [l for l in head.splitlines() if not re.match(r"(model|thinking):", l)]
        lines += [f"model: {model}", f"thinking: {thinking}"]
        updated = "\n".join(lines) + sep + body
        atomic_write(path, updated)
        with database(True) as db:
            record_edit(db, p["id"], "specialist " + sid, text, updated, actor, old_value=text)
        touched.append(sid)
    if touched:
        publish_specialist_change(p)
    return touched


def set_factory_model(p, name, args, actor):
    """factory-set-model: a model choice is policy kept in projects.factory_model_overrides, so it
    keeps the approval and never touches a live run. persist_to_toml edits machine.toml instead."""
    model = str(args.get("model") or "").strip()
    thinking = checked_thinking(args.get("thinking"))
    if args.get("persist_to_toml") is True or args.get("edit_role") is True:
        if model and model != "inherit":
            # A factory node names its model as provider/model, as pa model list prints it.
            checked_model("prime", model)
        draft = fsm_edit(p, "draft", {"name": name, "project": p["root"]})["draft"]
        try:
            payload = {"draft": draft, "state": args.get("state"), "model": model, "thinking": thinking}
            if args.get("role"):
                payload["role"] = args["role"]
            if args.get("edit_role"):
                payload["edit_role"] = True
            fsm_edit(p, "set-model", payload)
            fsm_edit(p, "save", {"draft": draft})
        except BaseException:
            try:
                fsm_edit(p, "discard", {"draft": draft})
            except (OSError, ValueError, subprocess.SubprocessError):
                pass
            raise
        with database(True) as db:
            record_edit(db, p["id"], "factory " + name, 0, model, actor)
        return f"Updated model for {args.get('state') or args.get('role') or 'default'} in machine.toml.", []
    if not model or model == "inherit":
        raise ValueError("Choose a model from pa model list; factory-clear-model-override reverts to machine.toml.")
    checked_model("prime", model)
    directory, manifest = factory_manifest(p, name)
    roles, states = model_targets(manifest, args)
    choice = {"model": model, **({"thinking": thinking} if thinking else {})}

    def change(overlay):
        for role in roles:
            overlay["roles"][role] = dict(choice)
        for state in states:
            overlay["states"][state] = dict(choice)
        checked_overlay(directory, name, overlay)

    with database(True) as db:
        row = db.execute("SELECT factory_model_overrides FROM projects WHERE id=?", (p["id"],)).fetchone()
        old = json.dumps(get_factory_model_overlay({"factory_model_overrides": row[0]}, name), sort_keys=True)
        overlay = update_factory_model_overlay(db, p["id"], name, change)
        record_edit(db, p["id"], "factory-model " + name, old, json.dumps(overlay, sort_keys=True), actor, old_value=old)
    synced = []
    if args.get("sync_specialists") is True:
        synced = sync_specialist_models(p, directory, roles, states, model, thinking, actor)
    target = "every role and state" if args.get("all") is True else (
        f"state {states[0]}" if states else f"role {roles[0]}")
    label = model + (" · " + thinking if thinking else "")
    return f"Set {target} of {name} to {label}. The approval stays; runs that start from now use it.", synced


def clear_model_override(p, name, args, actor):
    """factory-clear-model-override: back to the models in machine.toml (all, one role, or one state)."""
    def change(overlay):
        if args.get("all") is True:
            overlay["roles"].clear()
            overlay["states"].clear()
        elif args.get("state"):
            overlay["states"].pop(str(args["state"]), None)
        else:
            overlay["roles"].pop(str(args.get("role") or "default"), None)

    with database(True) as db:
        row = db.execute("SELECT factory_model_overrides FROM projects WHERE id=?", (p["id"],)).fetchone()
        old = json.dumps(get_factory_model_overlay({"factory_model_overrides": row[0]}, name), sort_keys=True)
        overlay = update_factory_model_overlay(db, p["id"], name, change)
        record_edit(db, p["id"], "factory-model " + name, old, json.dumps(overlay, sort_keys=True), actor, old_value=old)
    return "Cleared the model override; machine.toml models apply again."


def shown_factory(p, name, notice=None):
    """The review row: the graph and roles with the model overlay, its state, and liveness."""
    overlay = project_overlay(p["id"], name)
    row = factory_status(p, name, factory_approvals(p).get(name), graph=True, overlay=overlay)
    row["live"] = factory_live(p, name)
    row["model_overrides"] = overlay
    if notice:
        row["notice"] = notice
    with database() as db:
        publish_project_statuses(db, project_id=p["id"], factory_update=row)
    return row


def factory_action(op, p, args):
    """factory-validate, -show, -list, -approve, -preapprove, -delete, -design, -set-model,
    -clear-model-override, -set-default-model, and the compose ops."""
    actor = actor_kind()
    if actor == "crew" and op not in ("factory-validate", "factory-show", "factory-list"):
        raise ValueError("A crew thread cannot approve, delete, edit, or design a project factory.")
    if op == "factory-design":
        # The Captain may draft a factory when the plan needs a repeatable pipeline. A draft
        # never runs until the user approves its digest; a design turn does not start another.
        if actor == "captain" and captain_turn(p) == "design":
            raise ValueError("This turn is already designing a factory.")
        require_active(p)
        text = args.get("text")
        body = DESIGN_REQUEST + ("\n\n" + checked_text(text, 4000) if text else "")
        origin = args.get("request_id")
        with database(True) as db:
            active = db.execute(
                "SELECT id FROM jobs WHERE member=? AND purpose='design' AND status IN ('queued','running')",
                (p["lead"],),
            ).fetchone()
            if active:
                return {"job": active[0], "repeat": True}
            jid = queue(
                db,
                p["id"],
                p["lead"],
                "design",
                body,
                "design/" + p["id"] + "/" + checked_text(origin, 200) if origin else None,
            )
        launch_worker("run-job", jid)
        return {"job": jid}
    if op == "factory-list":
        return {"factories": project_factories(p)}
    if op == "factory-preapprove":
        return set_preapproval(p, args.get("name"), args.get("allow", True) is not False, lead_or_user(p, op))
    if op in COMPOSE_OPS:
        lead_or_user(p, op)
        with factory_lock(p["id"], checked_factory_name(args.get("name"))):
            return compose_action(op, p, args)
    name = checked_factory_name(args.get("name"))
    if op in LOCKED_OPS:
        with factory_lock(p["id"], name):
            return locked_factory_action(op, p, name, actor, args)
    approved = factory_approvals(p).get(name)
    if op == "factory-validate":
        row = factory_status(p, name, approved)
        with database() as db:
            publish_project_statuses(db, project_id=p["id"], factory_update=row)
        return row
    if op == "factory-show":
        return shown_factory(p, name)
    if op == "factory-delete":
        if actor == "captain":
            raise ValueError("Only the user deletes a project factory.")
        if factory_live(p, name):
            raise ValueError(f"A run of {name} is live. Stop it before you delete the factory.")
        directory = project_machines_dir(p) / name
        if directory.is_symlink():
            raise ValueError(f"{directory} leaves the project folder. Remove it yourself.")
        if directory.is_dir():
            shutil.rmtree(directory)
        with database(True) as db:
            db.execute(
                "DELETE FROM factory_approvals WHERE project=? AND name=?", (p["id"], name)
            )
            record_edit(db, p["id"], "factory " + name, approved or "", 0, actor)
        with database() as db:
            publish_project_statuses(db, project_id=p["id"], factory_removed=name)
        return {"ok": True, "name": name, "state": "missing"}
    raise ValueError("Choose a supported factory action.")


def locked_factory_action(op, p, name, actor, args):
    """The factory ops that change one factory; the caller holds its lock."""
    approved = factory_approvals(p).get(name)
    if op == "factory-approve":
        digest = args.get("digest")
        if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise ValueError("Pass the digest of the version to approve; factory-show returns it.")
        label = "user"
        if actor == "captain":
            if captain_turn(p) != "lead":
                raise ValueError(
                    "Only the user approves a project factory. Only a turn that answers a user "
                    "message can approve it. Ask the user with NEEDS YOU: to review it."
                )
            label = "captain-lead"
            if "preapprove" in args:
                set_preapproval(p, name, args["preapprove"] is True, label)
            # Without the user's standing pre-approval, a lead turn approves only the version it
            # started with, so it cannot approve its own edit.
            if not lead_preapproved(p, name):
                seen = turn_factory_digests(p).get(name)
                if seen is None or seen != digest:
                    raise ValueError(
                        f"{name} changed during this turn. Ask the user to review it in the "
                        "Captain panel (F6, then F), or to pre-approve edits of it."
                    )
        elif "preapprove" in args:
            set_preapproval(p, name, args["preapprove"] is True, label)
        row = factory_status(p, name, approved)
        if row["state"] == "missing":
            raise ValueError(f"No project factory named {name}.")
        if row["state"] == "invalid":
            raise ValueError(f"{name} is invalid. Fix it first: {row['errors'][0]}")
        if row["digest"] != digest:
            raise ValueError(f"{name} changed since this review. Review it again.")
        snapshot = store_factory_snapshot(factories_dir(p) / name, digest)
        with database(True) as db:
            db.execute(
                "INSERT OR REPLACE INTO factory_approvals(project,name,digest,actor,stamp) VALUES(?,?,?,?,?)",
                (p["id"], name, digest, actor, time.time()),
            )
            record_edit(db, p["id"], "factory " + name, approved or "", digest, label)
        with database() as db:
            publish_project_statuses(
                db,
                project_id=p["id"],
                factory_update={
                    "name": name,
                    "state": "approved",
                    "digest": digest,
                    "drift": row.get("drift") or "",
                },
            )
        return {"ok": True, "name": name, "digest": digest, "state": "approved", "by": actor,
                "snapshot": str(snapshot)}
    if op == "factory-set-default-model":
        if actor != "user":
            raise ValueError("Only the user edits a project factory model from review.")
        set_default_role(p, name, args)
        row = shown_factory(p, name)
        row["notice"] = (
            "Updated the default model. This version needs approval before it runs."
            if row["state"] in ("draft", "changed")
            else "Updated the default model."
        )
        return row
    if op == "factory-set-model":
        notice, synced = set_factory_model(p, name, args, lead_or_user(p, op))
        row = shown_factory(p, name)
        if row["state"] in ("draft", "changed"):
            notice += " This version needs approval before it runs."
        row["notice"] = notice
        if synced:
            row["synced_specialists"] = synced
        return row
    if op == "factory-clear-model-override":
        return shown_factory(p, name, clear_model_override(p, name, args, lead_or_user(p, op)))
    raise ValueError("Choose a supported factory action.")


def turn_digests_path(jid):
    return BASE / ("turn-" + str(uuid.UUID(jid)) + "-factories.json")


def record_turn_factories(jid, rows):
    """The factory digests a lead turn starts with; a Captain approval must match them."""
    write_json(turn_digests_path(jid), {r["name"]: r["digest"] for r in rows if r["digest"]})


def turn_factory_digests(p):
    with database() as db:
        row = db.execute(
            "SELECT id FROM jobs WHERE member=? AND status='running' ORDER BY created DESC LIMIT 1",
            (p["lead"],),
        ).fetchone()
    return (read_json(turn_digests_path(row[0])) or {}) if row else {}


def lead_factories(p):
    """(rows, error) for the Captain's instructions; read before any database lock."""
    try:
        return project_factories(p), None
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        return [], str(error)[:200]


def factory_menu(p, rows, error=None):
    """The project factory section of the Captain's instructions."""
    if error:
        listed = f" Project factories are unavailable: {error}."
    else:
        listed = "".join(f"\n- {r['name']} (project, {r['state']})" for r in rows)
        listed = f"\nProject factories:{listed}\n" if rows else " No project factories yet. "
    return (
        f"{listed}"
        f"You may write or change a project factory in {factories_dir(p)}/<name>/ (lowercase name, not a library machine name). "
        "Read print(rlm.factory.help()) first for the format. Run factory-validate(name) until it is valid; factory-show(name) returns the graph, digest, and state. "
        "The user approves one exact version in the Captain panel; any edit makes it a draft again. Python nodes run code, and the approval covers them. "
        "Never run a draft yourself, also not with rlm.factory.run: the runtime refuses project factories outside Captain runs. "
        "Never start an unapproved version (states other than approved). You may edit a factory while a run of it is live: a run executes from its own frozen copy. "
        "To change models or thinking (for example 'change all fsm subagents to X with high thinking'), call factory-set-model(name, model, thinking, all=true) or with role/state at once; it keeps the approval and never needs the user to wait. "
        "If the user pre-approves a structural change in their message, call factory-preapprove(name), edit, then factory-approve(name, digest) in the same turn. "
        "After you change a factory, tell the user under Needs you that it needs review. Start an approved one with delegate(machine=name,...). "
    )


# Chart-composed factories: one end-to-end machine from ordered stages. The TUI's chart builder
# previews the same wiring, so a change here changes captain_view.rs's compose_spec too.

COMPOSE_OPS = ("factory-compose", "factory-add-stage", "factory-remove-stage")
COMPOSE_STAGES = ("plan", "consensus", "implement", "sanitize-diff", "validate", "review-triad", "pr-ready")
COMPOSE_MODULES = ("sanitize-diff", "validate", "review-triad")
COMPOSE_FILE = "compose.json"
COMPOSE_PROMPTS = {
    "plan": "Write the architectural plan and the task breakdown for: {{task}}\n",
    "consensus": "Challenge this plan for: {{task}}\nApprove it only if it is sound; list what must change.\n",
    "implement": "Implement {{task}} in this worktree. Follow the plan and fix every finding you are given.\n"
                 "Emit a summary of the change and its diff.\n",
    "pr-ready": "Prepare the change for a pull request: a title, a summary, and how it was checked. Task: {{task}}\n",
}
COMPOSE_CONSENSUS = '''def vote(inputs):
    approve = bool(inputs.get("a")) and bool(inputs.get("b"))
    findings = "\\n\\n".join(str(inputs.get(k) or "") for k in ("a_findings", "b_findings")).strip()
    return {"verdict": "approve" if approve else "revise", "findings": findings}
'''
COMPOSE_DONE = '''def done(inputs):
    return {"report": str(inputs.get("summary") or "done")}
'''


COMPOSE_FANOUT = '''def start(inputs):
    return {"started": True}
'''


def compose_entry(stage):
    """The one state a stage starts at; fan-out stages start at a `<stage>-start` node."""
    return {
        "consensus": "consensus-start",
        "sanitize-diff": "sanitize-diff-sanitize",
        "validate": "validate-run",
        "review-triad": "review-triad-start",
    }.get(stage, stage)


def checked_stages(stages):
    if not isinstance(stages, list) or not stages:
        raise ValueError("Choose at least one stage.")
    for stage in stages:
        if stage not in COMPOSE_STAGES:
            raise ValueError(f"{stage!r} is not a stage. Choose from {', '.join(COMPOSE_STAGES)}.")
    if len(set(stages)) != len(stages):
        raise ValueError("Use each stage once.")
    if stages[0] not in ("plan", "implement"):
        raise ValueError("Start the factory with plan or implement.")
    if "sanitize-diff" in stages and ("implement" not in stages or stages.index("implement") > stages.index("sanitize-diff")):
        raise ValueError("Put implement before sanitize-diff: it cleans the implement diff.")
    return list(stages)


def compose_manifest(name, stages, models, library):
    """(machine.toml text, lock modules, module folders to copy) for ordered stages."""
    sections = [
        f'format = 1\nname = "{name}"\nversion = 1\n'
        f'description = {toml_value("End-to-end project pipeline: " + " -> ".join(stages) + ".")}\n',
        toml_section(("params", "task"), {"type": "text", "description": "What to build"}),
    ]
    lock, copies = {}, []

    def chosen(key):
        choice = models.get(key) or {}
        if not choice.get("model"):
            return {}
        out = {"model": checked_model("prime", choice["model"])[1]}
        thinking = checked_thinking(choice.get("thinking"))
        if thinking:
            out["thinking"] = thinking
        return out

    def add(sid, state):
        sections.append(toml_section(("states", sid), state))

    for i, stage in enumerate(stages):
        earlier = set(stages[:i])
        later = set(stages[i:])
        after = compose_entry(stages[i + 1]) if i + 1 < len(stages) else "done"
        rework = "implement" if "implement" in earlier else "plan" if "plan" in earlier else after
        head = {"entry": True} if i == 0 else {}
        if stage in ("plan", "implement", "pr-ready"):
            inputs = {}
            if stage == "plan" and "consensus" in stages:
                inputs["critique"] = "consensus-vote.findings?"
            if stage == "implement":
                if "plan" in earlier:
                    inputs["plan"] = "plan.plan"
                if "validate" in later:
                    inputs["checks"] = "validate-run.log?"
                if "review-triad" in later:
                    inputs["findings"] = "review-triad-verdict.findings?"
            if stage == "pr-ready":
                source = "implement.summary" if "implement" in earlier else "plan.plan" if "plan" in earlier else None
                if source:
                    inputs["summary"] = source
            outputs = {"plan": {"plan": "text"}, "implement": {"summary": "text", "diff": "text"}}.get(stage, {"summary": "text"})
            state = {**head, "max_entries": 4, "agent": {"prompt": f"prompts/{stage}.md", **chosen(stage)}}
            if inputs:
                state["inputs"] = inputs
            add(stage, {**state, "outputs": outputs, "next": after})
            continue
        if stage in ("consensus", "review-triad"):
            add(f"{stage}-start", {**head, "max_entries": 4, "kind": "python", "run": "nodes/fanout.py:start",
                                   "outputs": {"started": "bool"}})
        if stage == "consensus":
            sides = ["consensus-a", "consensus-b"]
            sections[-1] = sections[-1].rstrip("\n") + f"\nnext = {toml_value(sides)}\n"
            for sid in sides:
                state = {"max_entries": 4, "agent": {"prompt": "prompts/consensus.md", **(chosen(sid) or chosen(stage))}}
                if "plan" in earlier:
                    state["inputs"] = {"plan": "plan.plan"}
                add(sid, {**state, "outputs": {"approve": "bool", "findings": "text"}, "next": "consensus-vote"})
            add("consensus-vote", {
                "max_entries": 4, "kind": "python", "run": "nodes/consensus.py:vote", "join": sides,
                "inputs": {"a": "consensus-a.approve", "a_findings": "consensus-a.findings",
                           "b": "consensus-b.approve", "b_findings": "consensus-b.findings"},
                "outputs": {"verdict": "enum(approve, revise)", "findings": "text"},
                "route": {"on": "verdict", "approve": after, "revise": "plan" if "plan" in earlier else after},
            })
            continue
        module = library.get(stage)
        if not module or "error" in module:
            raise ValueError(f"The shared module {stage} is not available.")
        if stage == "sanitize-diff":
            wire = {"diff": "implement.diff", "next": after}
        elif stage == "validate":
            wire = {"pass": after, "fail": rework}
        else:
            subject = ("sanitize-diff-sanitize.diff" if "sanitize-diff" in earlier
                       else "implement.summary" if "implement" in earlier else "plan.plan")
            wire = {"subject": subject, "approve": after, "rework": rework}
            entries = [f"{stage}-{e}" for e in module["entry"]]
            sections[-1] = sections[-1].rstrip("\n") + f"\nnext = {toml_value(entries)}\n"
        states = module_states(module, stage, wire)
        # A review model chosen in the chart replaces every reviewer role of the module.
        review = chosen(stage) if stage == "review-triad" else {}
        module_roles = module["manifest"].get("roles") or {}
        roles = [r for r in module_roles if not (review and is_reviewer_role(r))]
        for role in module_roles:
            if review and is_reviewer_role(role):
                sections.append(toml_section(("roles", role), review))
            else:
                sections.append(toml_section(("roles", role), module_roles[role]))
        params = list(module["manifest"].get("params") or {})
        for param in params:
            sections.append(toml_section(("params", param), module["manifest"]["params"][param]))
        for sid, state in states.items():
            add(sid, {**head, **state} if sid == compose_entry(stage) else state)
        lock[stage] = {
            "module": module["name"], "version": module["version"], "source": module["source"],
            "digest": module["digest"], "states": sorted(states), "roles": roles, "params": params,
            "after": [], "wire": wire,
        }
        copies.append((module, stage))
    done = {"kind": "python", "run": "nodes/done.py:done", "outputs": {"report": "text"}}
    if "pr-ready" in stages:
        done["inputs"] = {"summary": "pr-ready.summary"}
    add("done", done)
    return "\n".join(sections), lock, copies


def is_reviewer_role(role):
    return role == "reviewer" or role.startswith("reviewer-")


def checked_models(models):
    if models is None:
        return {}
    if not isinstance(models, dict) or any(not isinstance(v, dict) for v in models.values()):
        raise ValueError("Pass models as an object of stage to {model, thinking}.")
    return models


def compose_factory(p, name, stages, models, replace=False):
    """Write <project>/.prime/machines/<name>/ from ordered stages; the user approves it."""
    name = checked_factory_name(name)
    stages = checked_stages(stages)
    models = checked_models(models)
    if name in installed_machines():
        raise ValueError(f"A library machine named {name} exists. Choose another name.")
    machines = project_machines_dir(p)
    directory = machines / name
    if directory.is_symlink():
        raise ValueError(f"{directory} is a symlink. Remove it yourself.")
    if directory.exists():
        if not replace:
            raise ValueError(f"The project factory {name} exists. Choose another name.")
    library = module_library() if set(stages) & set(COMPOSE_MODULES) else {}
    text, lock, copies = compose_manifest(name, stages, models, library)
    machines.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix="." + name + "-", dir=machines))
    try:
        (staging / "prompts").mkdir()
        (staging / "nodes").mkdir()
        for stage, prompt in COMPOSE_PROMPTS.items():
            if stage in stages:
                (staging / "prompts" / f"{stage}.md").write_text(prompt)
        if "consensus" in stages:
            (staging / "nodes/consensus.py").write_text(COMPOSE_CONSENSUS)
        if {"consensus", "review-triad"} & set(stages):
            (staging / "nodes/fanout.py").write_text(COMPOSE_FANOUT)
        (staging / "nodes/done.py").write_text(COMPOSE_DONE)
        for module, alias in copies:
            target = staging / "modules" / alias
            snapshot_files(Path(module["dir"]), target)
            (target / "module.toml").unlink()
        (staging / "machine.toml").write_text(text)
        if lock:
            write_lock(staging, {"modules": lock, "specialists": {}})
        (staging / COMPOSE_FILE).write_text(json.dumps({"stages": stages, "models": models}, indent=2) + "\n")
        check = subprocess.run([PA, "factory", "validate", str(staging)], capture_output=True, text=True,
                               timeout=60, stdin=subprocess.DEVNULL)
        if check.returncode:
            raise ValueError("The composed factory does not validate: "
                             + (check.stderr or check.stdout).replace(str(staging), name).strip()[-800:])
        if directory.exists():
            shutil.rmtree(directory)
        os.replace(staging, directory)
    finally:
        if staging.exists():
            shutil.rmtree(staging)
    return edited_factory(p, name, f"Composed {name}: {' -> '.join(stages)}.")


def composed_stages(p, name):
    directory = project_machines_dir(p) / checked_factory_name(name)
    try:
        saved = json.loads((directory / COMPOSE_FILE).read_text())
    except (OSError, ValueError):
        raise ValueError(f"{name} was not built from the chart. Press c to ask the Captain to change it, or b to build one.")
    return list(saved.get("stages") or []), dict(saved.get("models") or {})


def compose_action(op, p, args):
    require_active(p)
    if op == "factory-compose":
        return compose_factory(p, args.get("name"), args.get("stages"), args.get("models"), args.get("replace") is True)
    name = args.get("name")
    stages, models = composed_stages(p, name)
    stage = args.get("stage")
    if stage not in COMPOSE_STAGES:
        raise ValueError(f"Choose a stage from {', '.join(COMPOSE_STAGES)}.")
    if op == "factory-add-stage":
        if stage in stages:
            raise ValueError(f"{name} already has {stage}.")
        index = args.get("index")
        if not isinstance(index, int) or isinstance(index, bool):
            rank = COMPOSE_STAGES.index(stage)
            index = next((i for i, s in enumerate(stages) if COMPOSE_STAGES.index(s) > rank), len(stages))
        stages.insert(max(0, min(index, len(stages))), stage)
    else:
        if stage not in stages:
            raise ValueError(f"{name} has no {stage} stage.")
        if len(stages) == 1:
            raise ValueError("A factory keeps at least one stage. Delete it with D instead.")
        stages.remove(stage)
        models.pop(stage, None)
    return compose_factory(p, name, stages, models, replace=True)
