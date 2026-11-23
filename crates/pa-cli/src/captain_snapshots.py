"""Immutable factory snapshots addressed by digest, and per-run model overlays on a copy."""

import fcntl
import re
import shutil
import tomllib
from contextlib import contextmanager


def snapshot_dir(digest):
    """The folder that holds the factory files whose factory_digest is `digest`."""
    if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ValueError("A factory snapshot needs a SHA-256 digest.")
    return BASE / "snapshots" / digest


def verify_snapshot(path, digest):
    """True when the files at `path` still have `digest`."""
    try:
        return Path(path).is_dir() and factory_digest(path) == digest
    except (OSError, ValueError):
        return False


def store_factory_snapshot(src_dir, digest):
    """Copy `src_dir` into its snapshot folder once and return the folder. The source must have
    `digest`; an existing snapshot is reused only while it still verifies."""
    target = snapshot_dir(digest)
    if verify_snapshot(target, digest):
        return target
    staging = Path(tempfile.mkdtemp(dir=_snapshot_root(), prefix="." + digest[:12] + "-"))
    try:
        snapshot_files(Path(src_dir), staging)
        if factory_digest(staging) != digest:
            raise ValueError(f"The factory at {src_dir} does not have the digest {digest}.")
        if target.exists():
            shutil.rmtree(target)
        os.replace(staging, target)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    fsync_dir(target.parent)
    return target


def _snapshot_root():
    root = BASE / "snapshots"
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    return root


@contextmanager
def factory_lock(project_id, name):
    """Hold an exclusive lock on one project factory, so two edits of it never interleave."""
    locks = BASE / "locks"
    locks.mkdir(mode=0o700, parents=True, exist_ok=True)
    key = hashlib.sha256(f"{project_id}\0{name}".encode()).hexdigest()[:32]
    with open(locks / f"factory-{key}.lock", "a") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(handle, fcntl.LOCK_UN)


def factory_overrides(project_row):
    try:
        overrides = json.loads((project_row or {}).get("factory_model_overrides") or "{}")
    except (ValueError, TypeError):
        overrides = {}
    return overrides if isinstance(overrides, dict) else {}


def update_factory_model_overlay(db, project_id, factory_name, change):
    """Read, change, and write one factory's overlay in the open write transaction `db`.
    `change(overlay)` edits {roles, states} in place. Returns the new overlay."""
    row = db.execute("SELECT factory_model_overrides FROM projects WHERE id=?", (project_id,)).fetchone()
    overrides = factory_overrides({"factory_model_overrides": row[0] if row else "{}"})
    overlay = get_factory_model_overlay({"factory_model_overrides": json.dumps(overrides)}, factory_name)
    change(overlay)
    overlay = {k: {n: c for n, c in v.items() if _model_fields(c)} for k, v in overlay.items()}
    if any(overlay.values()):
        overrides[factory_name] = overlay
    else:
        overrides.pop(factory_name, None)
    db.execute(
        "UPDATE projects SET factory_model_overrides=? WHERE id=?",
        (json.dumps(overrides, sort_keys=True), project_id),
    )
    return overlay


def project_overlay(project_id, factory_name):
    """The overlay as stored now, not as the caller's project row last saw it."""
    with database() as db:
        row = db.execute("SELECT factory_model_overrides FROM projects WHERE id=?", (project_id,)).fetchone()
    return get_factory_model_overlay({"factory_model_overrides": row[0] if row else "{}"}, factory_name)


def get_factory_model_overlay(project_row, factory_name):
    """The user's model overrides for one factory: {roles: {...}, states: {...}}."""
    entry = factory_overrides(project_row).get(factory_name)
    entry = entry if isinstance(entry, dict) else {}
    overlay = {}
    for key in ("roles", "states"):
        names = entry.get(key)
        names = names if isinstance(names, dict) else {}
        overlay[key] = {k: v for k, v in names.items() if isinstance(v, dict)}
    return overlay


def _model_fields(choice):
    return {k: str(choice[k]) for k in ("model", "thinking") if choice.get(k)}


def apply_model_overlay(control_copy_dir, overlay_dict):
    """Set role and state models in the machine.toml of a run's copy, never the approved files.
    Returns the names changed. An unknown state is an error; a missing role is added."""
    overlay = overlay_dict or {}
    manifest = Path(control_copy_dir) / "machine.toml"
    text = manifest.read_text()
    changed = []
    for role, choice in sorted((overlay.get("roles") or {}).items()):
        fields = _model_fields(choice or {})
        if fields:
            text = _set_role_model(text, role, fields)
            changed.append("roles." + role)
    for state, choice in sorted((overlay.get("states") or {}).items()):
        fields = _model_fields(choice or {})
        if not fields:
            continue
        text = _set_state_model(text, state, fields)
        changed.append("states." + state)
    if changed:
        tomllib.loads(text)
        atomic_write_text(manifest, text)
    return changed


def _set_state_model(text, state, fields):
    """Set a state's model in whichever form machine.toml writes it: a `[states.<s>.agent]`
    table, a one-line `agent = { ... }`, or `model`/`thinking` keys in `[states.<s>]`."""
    lines = text.splitlines()
    table = section_bounds(lines, ("states", state, "agent"))
    if table:
        # An agent names a role or a model, not both; the chosen model replaces the role.
        quoted = toml_quoted_lines(lines)
        role = re.compile(r"^\s*role\s*=")
        lines = [l for i, l in enumerate(lines) if not (table[0] < i < table[1] and not quoted[i] and role.match(l))]
        table = section_bounds(lines, ("states", state, "agent"))
        for key, value in fields.items():
            lines = _set_table_key(lines, table, key, value)
            table = section_bounds(lines, ("states", state, "agent"))
        return "\n".join(lines) + "\n"
    bounds = section_bounds(lines, ("states", state))
    if not bounds:
        raise ValueError(f"machine.toml has no state named {state}.")
    quoted = toml_quoted_lines(lines)
    direct = re.compile(r"^\s*(model|thinking)\s*=")
    if any(not quoted[i] and direct.match(lines[i]) for i in range(bounds[0] + 1, bounds[1])):
        for key, value in fields.items():
            lines = _set_table_key(lines, bounds, key, value)
            bounds = section_bounds(lines, ("states", state))
        text = "\n".join(lines) + "\n"
        inline = re.compile(r"^\s*agent\s*=\s*\{")
        if not any(not quoted[i] and inline.match(lines[i]) for i in range(bounds[0] + 1, bounds[1])):
            return text
    index, agent = state_agent_line(text, state)
    agent.pop("role", None)
    agent.update(fields)
    lines = text.splitlines()
    lines[index] = "agent = " + toml_value(agent)
    return "\n".join(lines) + "\n"


def _set_role_model(text, role, fields):
    lines = text.splitlines()
    table = section_bounds(lines, ("roles", role))
    if table:
        for key, value in fields.items():
            lines = _set_table_key(lines, table, key, value)
            table = section_bounds(lines, ("roles", role))
        return "\n".join(lines) + "\n"
    roles = section_bounds(lines, ("roles",))
    line = f"{toml_key(role)} = {toml_value(fields)}"
    if not roles:
        return "\n".join(lines).rstrip("\n") + "\n\n[roles]\n" + line + "\n"
    pattern = re.compile(r"^\s*" + re.escape(toml_key(role)) + r"\s*=")
    quoted = toml_quoted_lines(lines)
    for index in range(roles[0] + 1, roles[1]):
        if not quoted[index] and pattern.match(lines[index]):
            existing = tomllib.loads(lines[index])[role]
            lines[index] = f"{toml_key(role)} = {toml_value(dict(existing, **fields))}"
            break
    else:
        lines.insert(roles[0] + 1, line)
    return "\n".join(lines) + "\n"


def _set_table_key(lines, bounds, key, value):
    start, end = bounds
    quoted = toml_quoted_lines(lines)
    pattern = re.compile(r"^\s*" + re.escape(key) + r"\s*=")
    line = f"{toml_key(key)} = {toml_value(value)}"
    for index in range(start + 1, end):
        if not quoted[index] and pattern.match(lines[index]):
            lines[index] = line
            return lines
    lines.insert(start + 1, line)
    return lines
