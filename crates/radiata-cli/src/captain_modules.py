"""Shared factory modules and project specialists, both vendored into a project factory.

A module is a factory fragment: a folder with `module.toml` (name, version, description,
entry and exit state names, `[wire]` placeholders, and its `[roles]`, `[params]`, and
`[states]`) plus its prompts, rubrics, and nodes. pa ships modules next to its bundled
machines (`rlm/modules/`); `<native home>/modules/` adds or overrides them by name.

A specialist is a project agent at `<project>/.prime/agents/<id>.md`: frontmatter `name`,
`description`, `model`, `thinking`, then the prompt body.

Both are materialized. Adding a module copies its files to `<factory>/modules/<alias>/` and
appends its states, renamed `<alias>-<state>`, to machine.toml. Using a specialist writes a
`[roles.<id>]` section and puts its prompt into each chosen state's prompt file between
markers. `<factory>/modules.lock` records the module versions and source digests and the
specialist digests. All of it is inside the factory folder, so the approval digest covers the
exact files, and every change here makes the factory a draft again.

Drift states, from modules.lock:
    module      current, update (the library has a newer version), or missing.
    specialist  current, changed (the project file changed), or missing.

modules.lock lists, for each module alias, the states, roles, and params the alias added and
the states it wired into its entry (`after`). An update removes all of them and adds the new
version again. Every file write is atomic, and an op that fails restores the factory folder.
"""

import stat

MODULE_OPS = (
    "factory-modules",
    "factory-add-module",
    "factory-update-module",
    "factory-use-specialist",
    "factory-update-specialist",
)
SPECIALIST_OPS = ("specialist-list", "specialist-show", "specialist-write", "specialist-delete")
LOCK_FILE = "modules.lock"
STATE_ID = re.compile(r"[a-z0-9][a-z0-9-]{0,63}")
BARE_KEY = re.compile(r"[A-Za-z0-9_-]+")
SPECIALIST_FIELD = re.compile(r"[^\n\r]{0,400}")
SPECIALIST_PROMPT_CAP = 32000


# TOML text helpers. machine.toml stays the Captain's text: these append sections, remove
# sections that pa wrote, and rewrite single lines, so comments and layout elsewhere stay.


def toml_string_end(line, start):
    """The index after the one-line string that starts at `line[start]`."""
    quote = line[start]
    index = start + 1
    while index < len(line):
        if quote == '"' and line[index] == "\\":
            index += 2
            continue
        if line[index] == quote:
            return index + 1
        index += 1
    return len(line)


def toml_quoted_lines(lines):
    """For each line, whether it starts inside a multi-line string (not a header or a key)."""
    inside = None
    out = []
    for line in lines:
        out.append(inside is not None)
        index = 0
        while index < len(line):
            if inside:
                if inside == '"""' and line[index] == "\\":
                    index += 2
                elif line.startswith(inside, index):
                    index += 3
                    for _ in range(2):  # a string may end with one or two extra quotes
                        if line.startswith(inside[0], index):
                            index += 1
                    inside = None
                else:
                    index += 1
                continue
            if line[index] == "#":
                break
            if line.startswith('"""', index) or line.startswith("'''", index):
                inside = line[index:index + 3]
                index += 3
            elif line[index] in "\"'":
                index = toml_string_end(line, index)
            else:
                index += 1
    return out


def toml_inline_end(line, start):
    """The index of the brace that closes the inline table at `line[start]`, or None."""
    depth = 0
    index = start
    while index < len(line):
        char = line[index]
        if char in "\"'":
            index = toml_string_end(line, index)
            continue
        if char == "#":
            return None
        if char in "{[":
            depth += 1
        elif char in "}]":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return None


def toml_key(key):
    return key if BARE_KEY.fullmatch(key) else json.dumps(key, ensure_ascii=False)


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(v) for v in value) + "]"
    if isinstance(value, dict):
        if not value:
            return "{}"
        return "{ " + ", ".join(f"{toml_key(k)} = {toml_value(v)}" for k, v in value.items()) + " }"
    raise ValueError(f"Cannot write {value!r} as TOML.")


def toml_section(path, table):
    lines = ["[" + ".".join(toml_key(p) for p in path) + "]"]
    lines += [f"{toml_key(k)} = {toml_value(v)}" for k, v in table.items()]
    return "\n".join(lines) + "\n"


HEADER = re.compile(r"^\s*\[(?!\[)(.*)\]\s*(?:#.*)?$")


def header_path(line):
    """The key path of a `[a.b]` table header line, or None."""
    match = HEADER.match(line)
    if not match:
        return None
    try:
        node = tomllib.loads("[" + match.group(1) + "]\n")
    except tomllib.TOMLDecodeError:
        return None
    path = []
    while isinstance(node, dict) and len(node) == 1:
        key = next(iter(node))
        path.append(key)
        node = node[key]
    return tuple(path)


def section_bounds(lines, path):
    """(start, end) line indexes of the `[path]` table, or None."""
    quoted = toml_quoted_lines(lines)

    def header(index):
        line = lines[index]
        return not quoted[index] and (HEADER.match(line) or line.lstrip().startswith("[["))

    for index, line in enumerate(lines):
        if not quoted[index] and header_path(line) == path:
            end = index + 1
            while end < len(lines) and not header(end):
                end += 1
            return index, end
    return None


def remove_sections(text, paths):
    """Drop every `[path]` table in `paths` and the pa comment line right above it."""
    lines = text.splitlines()
    for path in paths:
        bounds = section_bounds(lines, tuple(path))
        if bounds:
            start, end = bounds
            if start and lines[start - 1].startswith("# pa module ") or start and lines[start - 1].startswith("# pa specialist "):
                start -= 1
            del lines[start:end]
    return "\n".join(lines).rstrip("\n") + "\n"


def set_state_key(text, state, key, value):
    """Set one single-line key of `[states.<state>]`; insert it after the header when absent."""
    lines = text.splitlines()
    bounds = section_bounds(lines, ("states", state))
    if not bounds:
        raise ValueError(f"Write [states.{state}] as its own table in machine.toml first.")
    start, end = bounds
    quoted = toml_quoted_lines(lines)
    pattern = re.compile(r"^\s*" + re.escape(key) + r"\s*=")
    line = f"{toml_key(key)} = {toml_value(value)}"
    for index in range(start + 1, end):
        if not quoted[index] and pattern.match(lines[index]):
            lines[index] = line
            break
    else:
        lines.insert(start + 1, line)
    result = "\n".join(lines) + "\n"
    tomllib.loads(result)
    return result


def state_agent_line(text, state):
    """(line index, agent table) of a state's one-line `agent = { ... }`."""
    lines = text.splitlines()
    bounds = section_bounds(lines, ("states", state))
    if not bounds:
        raise ValueError(f"Write [states.{state}] as its own table in machine.toml first.")
    quoted = toml_quoted_lines(lines)
    for index in range(bounds[0] + 1, bounds[1]):
        match = re.match(r"^\s*agent\s*=\s*\{", lines[index])
        if quoted[index] or not match:
            continue
        start = match.end() - 1
        end = toml_inline_end(lines[index], start)
        if end is not None and re.fullmatch(r"\s*(?:#.*)?", lines[index][end + 1:]):
            return index, tomllib.loads("agent = " + lines[index][start:end + 1])["agent"]
        break
    raise ValueError(f"states.{state} needs a one-line agent = {{ ... }} table to use a specialist.")


# Modules


def module_roots():
    """(source, folder) in lookup order: bundled, then the user folder that overrides it."""
    bundled = bundled_modules_dir(PA)
    return ([("bundled", bundled)] if bundled else []) + [("user", NATIVE / "modules")]


def read_module(source, directory):
    """One module's metadata from its folder, or raise ValueError with the reason."""
    manifest = tomllib.loads((directory / "module.toml").read_text())
    name = manifest.get("name")
    if name != directory.name:
        raise ValueError(f"module.toml: name is {name!r}; it must be the folder name.")
    version = manifest.get("version")
    if not isinstance(version, int) or isinstance(version, bool) or version < 1:
        raise ValueError("module.toml: version must be a positive integer.")
    states = manifest.get("states")
    if not isinstance(states, dict) or not states:
        raise ValueError("module.toml: [states] is empty.")
    entry = manifest.get("entry")
    entry = [entry] if isinstance(entry, str) else entry
    if not isinstance(entry, list) or not entry or any(e not in states for e in entry):
        raise ValueError("module.toml: entry must name its states.")
    if manifest.get("exit") not in states:
        raise ValueError("module.toml: exit must name one of its states.")
    if "types" in manifest:
        raise ValueError("module.toml: a module cannot declare [types].")
    return {
        "name": name,
        "version": version,
        "description": str(manifest.get("description") or ""),
        "entry": entry,
        "exit": manifest["exit"],
        "wire": {
            k: v if isinstance(v, dict) else {"description": str(v)}
            for k, v in (manifest.get("wire") or {}).items()
        },
        "source": source,
        "dir": str(directory),
        "digest": factory_digest(directory),
        "manifest": manifest,
    }


def module_library():
    """Available modules by name; a user module overrides a bundled one with its name."""
    modules = {}
    for source, root in module_roots():
        if not root.is_dir():
            continue
        for directory in sorted(root.iterdir()):
            if not (directory / "module.toml").is_file() or not FACTORY_NAME.fullmatch(directory.name):
                continue
            try:
                modules[directory.name] = read_module(source, directory)
            except (OSError, ValueError) as error:
                modules[directory.name] = {"name": directory.name, "source": source, "error": str(error)[:300]}
    return modules


def read_lock(directory):
    """modules.lock, checked: anyone who can write the project can write this file."""
    path = Path(directory) / LOCK_FILE
    if not path.is_file():
        return {"modules": {}, "specialists": {}}
    lock = tomllib.loads(path.read_text())
    modules, specialists = lock.get("modules") or {}, lock.get("specialists") or {}
    if not isinstance(modules, dict) or not isinstance(specialists, dict):
        raise ValueError(f"{LOCK_FILE}: [modules] and [specialists] must be tables.")

    def names(entry, key):
        values = entry.get(key) or []
        if not isinstance(values, list) or not all(isinstance(v, str) for v in values):
            raise ValueError(f"{LOCK_FILE}: {key} must be a list of names.")
        return values

    for alias, entry in modules.items():
        if not FACTORY_NAME.fullmatch(alias) or not isinstance(entry, dict):
            raise ValueError(f"{LOCK_FILE}: {alias!r} is not a module alias.")
        for key in ("roles", "params", "after"):
            names(entry, key)
        for sid in names(entry, "states"):
            if not sid.startswith(alias + "-") or not STATE_ID.fullmatch(sid):
                raise ValueError(f"{LOCK_FILE}: the module {alias} lists {sid!r}, which is not an {alias}- state.")
    for sid, entry in specialists.items():
        if not FACTORY_NAME.fullmatch(sid) or not isinstance(entry, dict):
            raise ValueError(f"{LOCK_FILE}: {sid!r} is not a specialist id.")
        names(entry, "states")
    return {"modules": modules, "specialists": specialists}


def write_lock(directory, lock):
    text = "# Written by pa. The shared modules and project specialists this factory vendors.\n"
    for kind in ("modules", "specialists"):
        for key in sorted(lock[kind]):
            text += "\n" + toml_section((kind, key), lock[kind][key])
    atomic_write(Path(directory) / LOCK_FILE, text)


def atomic_write(path, data):
    """Replace `path` (never through a symlink) with a new file made in its folder with O_EXCL."""
    path = Path(path)
    data = data.encode() if isinstance(data, str) else data
    if path.exists() and not path.is_symlink():
        mode = stat.S_IMODE(path.stat().st_mode)
    else:
        umask = os.umask(0)
        os.umask(umask)
        mode = 0o666 & ~umask
    handle = tempfile.NamedTemporaryFile(dir=path.parent, prefix="." + path.name + "-", suffix=".tmp", delete=False)
    try:
        with handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(handle.name, mode)
        os.replace(handle.name, path)
    except BaseException:
        Path(handle.name).unlink(missing_ok=True)
        raise
    fsync_dir(path.parent)


@contextmanager
def restored_on_error(directory):
    """Run a factory edit; when it fails, put every file of the factory folder back."""
    directory = Path(directory)
    before = {rel: real.read_bytes() for rel, real in factory_files(directory)}
    folders = {str(Path(rel).parent) for rel in before} | {"."}
    try:
        yield
    except BaseException:
        for rel, _ in factory_files(directory):
            if rel not in before:
                (directory / rel).unlink()
        for rel, data in before.items():
            path = directory / rel
            if not path.is_file() or path.read_bytes() != data:
                path.parent.mkdir(parents=True, exist_ok=True)
                atomic_write(path, data)
        for current, subfolders, files in os.walk(directory, topdown=False):
            rel = os.path.relpath(current, directory)
            if rel not in folders and not subfolders and not files:
                os.rmdir(current)
        raise


def module_folder(directory, alias):
    """<factory>/modules/<alias>, refused when the alias or a symlink would leave the factory."""
    if not isinstance(alias, str) or not FACTORY_NAME.fullmatch(alias):
        raise ValueError("Name the module alias with lowercase letters, digits, and dashes.")
    modules = Path(directory) / "modules"
    target = modules / alias
    for part in (modules, target):
        if part.is_symlink():
            raise ValueError(f"{part} is a symlink. Remove it yourself.")
    if modules.exists() and modules.resolve() != Path(directory).resolve() / "modules":
        raise ValueError(f"{modules} leaves the factory folder.")
    return target


def wired(value, wire, declared):
    """A `@name` placeholder resolved from `wire`; None when an optional wire is not given."""
    name = value[1:].rstrip("?")
    if name not in declared:
        raise ValueError(f"The module uses @{name}, which its [wire] does not declare.")
    if name not in wire:
        return None
    return wire[name] + ("?" if value.endswith("?") and not wire[name].endswith("?") else "")


def module_states(module, alias, wire):
    """The module's states renamed `<alias>-<state>`, with paths and wires resolved."""
    states = module["manifest"]["states"]
    declared = module["wire"]
    rename = {s: f"{alias}-{s}" for s in states}

    def target(value):
        if value.startswith("@"):
            return wired(value, wire, declared)
        return rename.get(value, value)

    def path(value):
        return f"modules/{alias}/{value}"

    out = {}
    for sid, raw in states.items():
        state = {k: v for k, v in raw.items() if k != "entry"}
        if isinstance(state.get("agent"), dict):
            agent = dict(state["agent"])
            for key in ("prompt", "rubric"):
                if isinstance(agent.get(key), str):
                    agent[key] = path(agent[key])
            state["agent"] = agent
        if isinstance(state.get("run"), str):
            state["run"] = path(state["run"])
        if isinstance(state.get("inputs"), dict):
            inputs = {}
            for port, source in state["inputs"].items():
                if source.startswith("@"):
                    source = wired(source, wire, declared)
                    if source is None:
                        continue
                elif source.startswith("assets/"):
                    source = path(source)
                else:
                    head, _, rest = source.partition(".")
                    source = f"{rename.get(head, head)}.{rest}" if rest else source
                inputs[port] = source
            state["inputs"] = inputs
        if isinstance(state.get("next"), str):
            state["next"] = target(state["next"])
            if state["next"] is None:
                del state["next"]
        elif isinstance(state.get("next"), list):
            state["next"] = [t for t in map(target, state["next"]) if t is not None]
        if isinstance(state.get("route"), dict):
            route = {"on": state["route"]["on"]}
            for key, value in state["route"].items():
                if key != "on":
                    resolved = target(value)
                    if resolved is None:
                        raise ValueError(f"Wire {value[1:]}: the route of {sid} needs every target.")
                    route[key] = resolved
            state["route"] = route
        if isinstance(state.get("join"), list):
            state["join"] = [rename.get(s, s) for s in state["join"]]
        out[rename[sid]] = state
    return out


def vendor_module(directory, module, alias, wire, after):
    """Copy a module into a factory folder and merge its states; return its lock entry.

    The caller holds the factory folder; it is a draft until the user approves it again.
    """
    directory = Path(directory)
    target = module_folder(directory, alias)
    if not isinstance(wire, dict) or any(not isinstance(v, str) or not v for v in wire.values()):
        raise ValueError("Pass wire as an object of placeholder names to state or port names.")
    unknown = sorted(set(wire) - set(module["wire"]))
    if unknown:
        raise ValueError(f"{module['name']} has no wire named {', '.join(unknown)}.")
    missing = sorted(
        k for k, spec in module["wire"].items() if not spec.get("optional") and k not in wire
    )
    if missing:
        raise ValueError(f"Wire {', '.join(missing)} to add {module['name']}.")
    after = [after] if isinstance(after, str) else list(after or [])
    text = (directory / "machine.toml").read_text()
    manifest = tomllib.loads(text)
    states = module_states(module, alias, wire)
    clash = sorted(set(states) & set(manifest.get("states") or {}))
    if clash:
        raise ValueError(f"machine.toml already has {', '.join(clash)}. Choose another alias.")
    for sid in states:
        if not STATE_ID.fullmatch(sid):
            raise ValueError(f"{sid} is not a valid state id. Choose a shorter alias.")
    for sid in after:
        if sid not in (manifest.get("states") or {}):
            raise ValueError(f"machine.toml has no state {sid} to wire into {module['name']}.")
        if "route" in manifest["states"][sid]:
            raise ValueError(f"{sid} routes by its output. Point a route value at the module yourself.")
    added = f"\n# pa module {alias}: {module['name']} v{module['version']}\n"
    roles = manifest.get("roles") or {}
    added_roles, added_params = [], []
    for role, spec in (module["manifest"].get("roles") or {}).items():
        if role not in roles:
            added += toml_section(("roles", role), spec)
            added_roles.append(role)
    params = manifest.get("params") or {}
    for param, spec in (module["manifest"].get("params") or {}).items():
        if param in params and params[param] != spec:
            raise ValueError(f"machine.toml has a different param {param}. Rename one of them.")
        if param not in params:
            added += toml_section(("params", param), spec)
            added_params.append(param)
    for sid, state in states.items():
        added += toml_section(("states", sid), state)
    text = text.rstrip("\n") + "\n" + added
    entries = [f"{alias}-{e}" for e in module["entry"]]
    for sid in after:
        text = set_state_key(text, sid, "next", entries[0] if len(entries) == 1 else entries)
    tomllib.loads(text)
    if target.exists():
        shutil.rmtree(target)
    snapshot_files(Path(module["dir"]), target)
    (target / "module.toml").unlink()
    atomic_write(directory / "machine.toml", text)
    return {
        "module": module["name"],
        "version": module["version"],
        "source": module["source"],
        "digest": module["digest"],
        "states": sorted(states),
        "roles": added_roles,
        "params": added_params,
        "after": after,
        "wire": wire,
    }


def unvendor_module(directory, alias, entry):
    """Remove a vendored module: its files and the states, roles, and params it added."""
    directory = Path(directory)
    target = module_folder(directory, alias)
    text = (directory / "machine.toml").read_text()
    text = remove_sections(
        text,
        [("states", s) for s in entry.get("states") or []]
        + [("roles", r) for r in entry.get("roles") or []]
        + [("params", p) for p in entry.get("params") or []],
    )
    atomic_write(directory / "machine.toml", text)
    if target.is_dir():
        shutil.rmtree(target)


def module_drift(lock, modules):
    rows = []
    for alias, entry in sorted(lock["modules"].items()):
        latest = modules.get(entry.get("module"))
        version = entry.get("version")
        if not latest or "error" in latest:
            state, newest = "missing", None
        elif latest["version"] > (version or 0) or (
            latest["version"] == version and latest["digest"] != entry.get("digest")
        ):
            state, newest = "update", latest["version"]
        else:
            state, newest = "current", latest["version"]
        rows.append({"alias": alias, "module": entry.get("module"), "version": version,
                     "latest": newest, "state": state})
    return rows


# Specialists


def agents_dir(p):
    """<project>/.prime/agents, refused when it leaves the project folder."""
    root = Path(p["root"]).resolve()
    directory = root / ".prime" / "agents"
    for part in (root / ".prime", directory):
        if part.is_symlink() or (part.exists() and not part.resolve().is_relative_to(root)):
            raise ValueError(f"{part} leaves the project folder.")
    return directory


def specialist_path(p, sid):
    if not isinstance(sid, str) or not FACTORY_NAME.fullmatch(sid):
        raise ValueError("Name a specialist with lowercase letters, digits, and dashes (at most 64).")
    directory = agents_dir(p)
    path = directory / (sid + ".md")
    if path.is_symlink() or (path.exists() and not path.resolve().parent == directory.resolve()):
        raise ValueError(f"{path} is a symlink or leaves {directory}.")
    return path


def parse_specialist(sid, text):
    fields = {"name": sid, "description": "", "model": "", "thinking": ""}
    body = text
    if text.startswith("---\n"):
        head, sep, rest = text[4:].partition("\n---\n")
        if sep:
            body = rest
            for line in head.splitlines():
                key, colon, value = line.partition(":")
                if colon and key.strip() in fields:
                    fields[key.strip()] = value.strip()
    return {"id": sid, **fields, "prompt": body.strip("\n")}


def read_specialist(p, sid):
    path = specialist_path(p, sid)
    if not path.is_file():
        raise ValueError(f"No specialist named {sid}.")
    raw = path.read_bytes()
    row = parse_specialist(sid, raw.decode("utf-8", errors="replace"))
    row["digest"] = hashlib.sha256(raw).hexdigest()
    row["path"] = str(path)
    return row


def project_specialists(p, prompt_cap=8000):
    """Every readable specialist of `p`; the prompt is capped for the panel."""
    try:
        directory = agents_dir(p)
    except ValueError:
        return []
    if not directory.is_dir():
        return []
    rows = []
    for path in sorted(directory.glob("*.md")):
        sid = path.stem
        if not FACTORY_NAME.fullmatch(sid):
            continue
        try:
            row = read_specialist(p, sid)
        except (OSError, ValueError):
            continue
        row["prompt"] = row["prompt"][:prompt_cap]
        rows.append(row)
    return rows


def specialist_digest(p, sid):
    try:
        path = specialist_path(p, sid)
        return hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None
    except (OSError, ValueError):
        return None


def specialist_drift(p, lock):
    rows = []
    for sid, entry in sorted(lock["specialists"].items()):
        digest = specialist_digest(p, sid)
        state = "missing" if digest is None else "current" if digest == entry.get("digest") else "changed"
        rows.append({"id": sid, "states": entry.get("states") or [], "state": state})
    return rows


def specialist_block(sid, prompt):
    return f"<!-- pa specialist {sid} -->\n{prompt}\n<!-- end pa specialist {sid} -->\n\n"


def put_specialist_block(text, sid, prompt):
    """Put the specialist's prompt at the top of a state prompt, replacing an older copy."""
    begin, end = f"<!-- pa specialist {sid} -->\n", f"<!-- end pa specialist {sid} -->\n"
    start = text.find(begin)
    if start >= 0:
        stop = text.find(end, start)
        if stop < 0:
            raise ValueError(f"The specialist {sid} block has no end marker.")
        stop += len(end)
        while text[stop:stop + 1] == "\n":
            stop += 1
        text = text[:start] + text[stop:]
    return specialist_block(sid, prompt) + text


def vendor_specialist(directory, specialist, states, lock):
    """Materialize a specialist into a factory: its role, and its prompt in each state's prompt."""
    directory = Path(directory)
    sid = specialist["id"]
    if not specialist["model"]:
        raise ValueError(f"Set a model for the specialist {sid} to use it in a factory.")
    text = (directory / "machine.toml").read_text()
    manifest = tomllib.loads(text)
    if sid in (manifest.get("roles") or {}) and sid not in lock["specialists"]:
        raise ValueError(f"machine.toml has a role named {sid}. Rename it or the specialist.")
    if not states:
        raise ValueError("Name the agent states that use the specialist.")
    root = directory.resolve()
    prompts = []
    for state in states:
        spec = (manifest.get("states") or {}).get(state)
        if not isinstance(spec, dict) or spec.get("kind") == "python":
            raise ValueError(f"{state} is not an agent state of this factory.")
        index, agent = state_agent_line(text, state)
        relative = agent.get("prompt")
        if not isinstance(relative, str) or not relative:
            raise ValueError(f"states.{state} has no prompt file for the specialist.")
        prompt = directory / relative
        if prompt.is_symlink() or not prompt.resolve().is_relative_to(root) or not prompt.is_file():
            raise ValueError(f"{relative} is not a prompt file inside the factory folder.")
        agent = {k: v for k, v in agent.items() if k not in ("model", "thinking")}
        agent["role"] = sid
        lines = text.splitlines()
        lines[index] = f"agent = {toml_value(agent)}"
        text = "\n".join(lines) + "\n"
        prompts.append(prompt)
    text = remove_sections(text, [("roles", sid)])
    role = {"model": specialist["model"]}
    if specialist["thinking"]:
        role["thinking"] = specialist["thinking"]
    text = text.rstrip("\n") + f"\n\n# pa specialist {sid}\n" + toml_section(("roles", sid), role)
    tomllib.loads(text)
    for prompt in dict.fromkeys(prompts):
        atomic_write(prompt, put_specialist_block(prompt.read_text(), sid, specialist["prompt"]))
    atomic_write(directory / "machine.toml", text)
    return {"digest": specialist["digest"], "states": sorted(set(states))}


def factory_lock_rows(p, snapshot):
    """(modules, specialists, drift text) of a factory snapshot for its review and status."""
    try:
        lock = read_lock(snapshot)
    except (OSError, ValueError) as error:
        return [], [], f"{LOCK_FILE} does not parse: {str(error)[:120]}"
    modules = module_drift(lock, module_library() if lock["modules"] else {})
    specialists = specialist_drift(p, lock)
    if any(s["state"] == "missing" for s in specialists):
        # factory-update-specialist needs the file; writing it again restores the update path.
        drift = "specialist missing - write it again"
    elif any(s["state"] == "changed" for s in specialists):
        drift = "specialist changed - update"
    elif any(m["state"] == "update" for m in modules):
        drift = "module update available"
    else:
        drift = ""
    return modules, specialists, drift


# Actions


def editable_factory(p, name):
    """The folder of a project factory the caller may edit now. A live run keeps its own copy."""
    name = checked_factory_name(name)
    directory = project_machines_dir(p) / name
    if directory.is_symlink() or not (directory / "machine.toml").is_file():
        raise ValueError(f"No project factory named {name} in this project.")
    factory_files(directory)  # refuses a symlink that leaves the folder
    return directory


def edited_factory(p, name, notice):
    """The factory row after an edit, published to the status file."""
    row = factory_status(p, name, factory_approvals(p).get(name))
    if row["state"] in ("draft", "changed"):
        notice += " This version needs approval before it runs."
    row["notice"] = notice
    with database(True) as db:
        record_edit(db, p["id"], "factory " + name, 0, row["digest"], actor_kind())
    with database() as db:
        publish_project_statuses(db, project_id=p["id"], factory_update=row)
    return row


def module_action(op, p, args):
    """Module and specialist ops. Every factory edit makes the factory a draft again."""
    actor = actor_kind()
    if op in ("factory-modules", "specialist-list", "specialist-show"):
        if op == "factory-modules":
            return {"modules": [
                {k: v for k, v in m.items() if k != "manifest"} for m in module_library().values()
            ]}
        if op == "specialist-list":
            return {"specialists": [
                {k: v for k, v in s.items() if k != "prompt"} for s in project_specialists(p)
            ]}
        row = read_specialist(p, args.get("id"))
        row["used_by"] = [
            f["name"] for f in project_factories(p) if any(s["id"] == row["id"] for s in f.get("specialists") or [])
        ]
        return row
    if actor == "crew":
        raise ValueError("A crew thread cannot change project factories or specialists.")
    if op == "specialist-delete":
        if actor != "user":
            raise ValueError("Only the user deletes a specialist.")
        path = specialist_path(p, args.get("id"))
        if not path.is_file():
            raise ValueError(f"No specialist named {args.get('id')}.")
        path.unlink()
        publish_specialist_change(p)
        return {"ok": True, "id": args["id"]}
    if op == "specialist-write":
        # A factory that uses a changed specialist drifts, so the user still approves it.
        if actor == "captain" and captain_turn(p) not in ("lead", "design", "digest", "brief"):
            raise ValueError(
                "The Captain writes a specialist only on a turn that answers the user, designs a factory, "
                "or reviews crew reports."
            )
        sid = args.get("id")
        path = specialist_path(p, sid)
        fields = {}
        for key in ("name", "description", "model", "thinking"):
            value = str(args.get(key) or "").strip()
            if not SPECIALIST_FIELD.fullmatch(value):
                raise ValueError(f"Keep the specialist {key} on one line (at most 400 characters).")
            fields[key] = value
        fields["name"] = fields["name"] or sid
        fields["thinking"] = checked_thinking(fields["thinking"])
        if fields["model"] and not re.fullmatch(r"[A-Za-z0-9._:/@+-]{1,200}", fields["model"]):
            raise ValueError("Name the specialist model as provider/model, for example anthropic/claude-opus-5-5.")
        prompt = args.get("prompt")
        if not isinstance(prompt, str) or not prompt.strip() or len(prompt.encode()) > SPECIALIST_PROMPT_CAP:
            raise ValueError(f"Write a specialist prompt between 1 and {SPECIALIST_PROMPT_CAP} bytes.")
        if "<!-- pa specialist" in prompt or "<!-- end pa specialist" in prompt:
            raise ValueError("The specialist prompt cannot contain pa specialist markers.")
        path.parent.mkdir(parents=True, exist_ok=True)
        text = "---\n" + "".join(f"{k}: {v}\n" for k, v in fields.items()) + "---\n\n" + prompt.strip() + "\n"
        atomic_write(path, text)
        with database(True) as db:
            record_edit(db, p["id"], "specialist " + sid, 0, text, actor)
        publish_specialist_change(p)
        return read_specialist(p, sid)
    if op not in MODULE_OPS[1:]:
        raise ValueError("Choose a supported module or specialist action.")
    with factory_lock(p["id"], checked_factory_name(args.get("name"))):
        directory = editable_factory(p, args.get("name"))
        with restored_on_error(directory):
            notice = edit_factory_parts(op, p, directory, args)
        return edited_factory(p, directory.name, notice)


def publish_specialist_change(p):
    """Republish the project's factories: those that use a changed specialist show drift."""
    rows, error = lead_factories(p)
    if error:
        return  # the status keeps the factories it last published
    with database() as db:
        publish_project_statuses(db, project_id=p["id"], factories_by_project={p["id"]: rows})


def edit_factory_parts(op, p, directory, args):
    """Vendor or update a module or a specialist in a factory folder; return the notice."""
    name = directory.name
    lock = read_lock(directory)
    if op in ("factory-add-module", "factory-update-module"):
        if op == "factory-add-module":
            modules = module_library()
            module = modules.get(args.get("module"))
            if not module:
                raise ValueError(f"No module named {args.get('module')}. See factory-modules.")
            if "error" in module:
                raise ValueError(f"The module {module['name']} is invalid: {module['error']}")
            alias = args.get("alias") or module["name"]
            module_folder(directory, alias)
            if alias in lock["modules"]:
                raise ValueError(f"{name} already vendors {alias}. Use factory-update-module.")
            entry = vendor_module(directory, module, alias, args.get("wire") or {}, args.get("after"))
            notice = f"Added {module['name']} v{module['version']} as {alias}."
        else:
            alias = args.get("alias")
            module_folder(directory, alias)
            if alias not in lock["modules"]:
                raise ValueError(f"{name} does not vendor a module named {alias}.")
            old = lock["modules"][alias]
            module = module_library().get(old.get("module"))
            if not module or "error" in module:
                raise ValueError(f"The module {old.get('module')} is not available.")
            # The caller restores the folder when the update fails part way.
            unvendor_module(directory, alias, old)
            entry = vendor_module(directory, module, alias, old.get("wire") or {}, old.get("after") or [])
            notice = f"Updated {alias} to {module['name']} v{module['version']}."
        lock["modules"][alias] = entry
        write_lock(directory, lock)
        return notice
    if op in ("factory-use-specialist", "factory-update-specialist"):
        sid = args.get("specialist")
        specialist = read_specialist(p, sid)
        if op == "factory-update-specialist":
            if sid not in lock["specialists"]:
                raise ValueError(f"{name} does not use the specialist {sid}.")
            states = lock["specialists"][sid].get("states") or []
        else:
            states = args.get("states") or []
            states = [states] if isinstance(states, str) else states
            states = sorted(set(states) | set((lock["specialists"].get(sid) or {}).get("states") or []))
        lock["specialists"][sid] = vendor_specialist(directory, specialist, states, lock)
        write_lock(directory, lock)
        return f"Materialized the specialist {sid}."
    raise ValueError("Choose a supported module or specialist action.")


def specialist_menu(p):
    """The specialist section of the Captain's instructions."""
    rows = project_specialists(p, prompt_cap=0)
    listed = "".join(f"\n- {s['id']}: {s['description'] or s['name']}" for s in rows)
    return (
        (f"\nProject specialists:{listed}\n" if rows else " No project specialists yet. ")
        + "Write a specialist with specialist-write(id,name,description,model,thinking,prompt) when the project needs an expert role, for example a theory verifier. "
        "Give one to a crew thread with delegate(specialist=id,...); put one in a factory state with factory-use-specialist(name,specialist,states). "
        "factory-modules lists shared parts such as web-research, review-triad, sanitize-diff, and validate; add one with factory-add-module(name,module,wire,after). "
    )
