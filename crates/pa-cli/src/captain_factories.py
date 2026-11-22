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


def factory_status(p, name, approved=None, library=None, graph=False):
    """One project factory's state, digest, and errors. Its own content never raises here.

    The folder is copied once into a temporary snapshot, and the digest, the validation, and
    the compiled graph all come from that snapshot, so they describe one version.
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
        if name in (installed_machines() if library is None else library):
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
        if graph and not errors:
            row["graph"], row["graph_error"] = factory_graph(snapshot, manifest or {})
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
    return [factory_status(p, n, approvals.get(n), library) for n in names]


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
    approved one. Then the run reads only files the user approved.
    """
    target = control / "machine" / spec["name"]
    if target.exists():
        shutil.rmtree(target)
    target.parent.mkdir(parents=True, exist_ok=True)
    snapshot_files(Path(spec["dir"]), target)
    copied = factory_digest(target)
    approved = factory_approvals(p).get(spec["name"])
    if copied != spec["digest"] or approved != spec["digest"]:
        raise ValueError(
            f"NEEDS YOU: {spec['name']} changed after it was approved. "
            "Review and approve it again in the Captain panel."
        )
    return target


def factory_action(op, p, args):
    """factory-validate, factory-show, factory-approve, factory-delete, factory-design."""
    actor = actor_kind()
    if actor == "crew" and op not in ("factory-validate", "factory-show"):
        raise ValueError("A crew thread cannot approve, delete, or design a project factory.")
    if op == "factory-design":
        if actor == "captain":
            raise ValueError("The user requests a factory design.")
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
    name = checked_factory_name(args.get("name"))
    approved = factory_approvals(p).get(name)
    if op == "factory-validate":
        return factory_status(p, name, approved)
    if op == "factory-show":
        row = factory_status(p, name, approved, graph=True)
        row["live"] = factory_live(p, name)
        return row
    if op == "factory-approve":
        digest = args.get("digest")
        if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise ValueError("Pass the digest of the version to approve; factory-show returns it.")
        if actor == "captain":
            if captain_turn(p) != "lead":
                raise ValueError(
                    "Only the user approves a project factory. Only a turn that answers a user "
                    "message can approve it. Ask the user with NEEDS YOU: to review it."
                )
            seen = turn_factory_digests(p).get(name)
            if seen is None or seen != digest:
                raise ValueError(
                    f"{name} changed during this turn. Ask the user to review it in the "
                    "Captain panel (F6, then F)."
                )
        row = factory_status(p, name, approved)
        if row["state"] == "missing":
            raise ValueError(f"No project factory named {name}.")
        if row["state"] == "invalid":
            raise ValueError(f"{name} is invalid. Fix it first: {row['errors'][0]}")
        if row["digest"] != digest:
            raise ValueError(f"{name} changed since this review. Review it again.")
        with database(True) as db:
            db.execute(
                "INSERT OR REPLACE INTO factory_approvals VALUES(?,?,?,?,?)",
                (p["id"], name, digest, actor, time.time()),
            )
            db.execute(
                "INSERT INTO edits VALUES(?,?,?,?,?,?)",
                (p["id"], "factory " + name, len(approved or ""), len(digest), actor, time.time()),
            )
        return {"ok": True, "name": name, "digest": digest, "state": "approved", "by": actor}
    if op == "factory-delete":
        if actor == "captain":
            raise ValueError("Only the user deletes a project factory.")
        if factory_live(p, name):
            raise ValueError(f"A run of {name} is live. Stop it before you delete the factory.")
        directory = factories_dir(p) / name
        root = Path(p["root"]).resolve()
        if directory.is_symlink() or (
            directory.exists()
            and not directory.resolve().is_relative_to(factories_dir(p).resolve())
        ) or not factories_dir(p).resolve().is_relative_to(root):
            raise ValueError(f"{directory} leaves the project folder. Remove it yourself.")
        if directory.is_dir():
            shutil.rmtree(directory)
        with database(True) as db:
            db.execute(
                "DELETE FROM factory_approvals WHERE project=? AND name=?", (p["id"], name)
            )
            db.execute(
                "INSERT INTO edits VALUES(?,?,?,?,?,?)",
                (p["id"], "factory " + name, len(approved or ""), 0, actor, time.time()),
            )
        return {"ok": True, "name": name, "state": "missing"}
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
        "Never start an unapproved version (states other than approved), and never edit a factory while a run of it is live. "
        "After you change a factory, tell the user under Needs you that it needs review. Start an approved one with delegate(machine=name,...). "
    )
