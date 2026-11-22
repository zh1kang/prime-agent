"""Machine definition drafts for pa's native machine editor.

`pa thread fsm-edit OP` reads one JSON object on stdin and prints one JSON object.
A draft is a working copy in `machine-drafts` beside the personal machines folder. Saving validates it with the
factory library validator first, so a failed edit never changes an installed machine.

A project factory (`<project>/.prime/machines/<name>/`) drafts with draft(name, project). Saving it
replaces its machine.toml in place, so its digest changes and the Captain needs approval again.

Operations: list, draft(name, target?, project?), states(draft), set-model(draft, state, model),
read(draft), write(draft, text), save(draft), discard(draft).
"""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib

pa = sys.argv[1]
SPEC = re.compile(r"(?ms)^\x60\x60\x60machine-spec\s*\n(.*?)^\x60\x60\x60\s*$")


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def drafts_root():
    # Beside the machines folder, never inside it: the catalog would list a draft as a machine.
    return personal_machines().parent / "machine-drafts"


def meta_path(draft):
    return Path(draft) / ".pa-draft.json"


def load_draft(args):
    draft = Path(args["draft"]).resolve()
    if draft.parent != drafts_root().resolve() or not draft.name.startswith(".pa-edit-"):
        raise ValueError("Unknown draft.")
    return draft, json.loads(meta_path(draft).read_text())


def md_spec(text):
    block = SPEC.search(text)
    try:
        spec = json.loads(block[1]) if block else {}
    except ValueError:
        spec = {}
    states = spec.get("states", spec.get("nodes", [])) if isinstance(spec, dict) else []
    if not isinstance(states, list) or not all(isinstance(s, dict) and "id" in s for s in states):
        states = []
    return block, spec, states


def states(meta):
    """(id, model, editable) rows for the draft's states."""
    path = Path(meta["file"])
    if meta["kind"] == "md":
        _, _, rows = md_spec(path.read_text())
        out = []
        for state in rows:
            agent = state.get("subagent")
            editable = isinstance(agent, dict)
            out.append({"id": str(state["id"]), "model": agent.get("model", "inherit") if editable else "see definition", "editable": editable})
        return out
    try:
        manifest = tomllib.loads(path.read_text())
    except tomllib.TOMLDecodeError as error:
        return [{"id": "machine.toml", "model": "does not parse: " + str(error)[:120], "editable": False}]
    out = []
    for name, state in (manifest.get("states") or {}).items():
        agent = state.get("agent") if isinstance(state, dict) else None
        if isinstance(agent, dict):
            model = agent.get("model", "inherit")
        else:
            model = (state.get("kind") if isinstance(state, dict) else None) or "-"
        out.append({"id": name, "model": model, "editable": False})
    return out


def placeholder(spec):
    """A value that type-checks for a required param, so a draft compiles for its graph."""
    kind = str(spec.get("type", "text")) if isinstance(spec, dict) else "text"
    if kind in ("text", "file"):
        return None
    if kind.startswith("list["):
        return "[]"
    if kind.startswith("enum("):
        return json.dumps(kind[5:-1].split(",")[0].strip())
    return {"int": "0", "float": "0", "bool": "false"}.get(kind, "null")


def graph(meta):
    """(compiled spec, error) for the draft, from `pa factory compile`."""
    target = meta["dir"] if meta["kind"] == "toml" else meta["file"]
    args = []
    if meta["kind"] == "toml":
        try:
            params = tomllib.loads(Path(meta["file"]).read_text()).get("params", {})
        except tomllib.TOMLDecodeError as error:
            return None, str(error)
        for name, spec in params.items():
            if isinstance(spec, dict) and "default" not in spec:
                value = placeholder(spec)
                args += ["--param", f"{name}={'{' + name + '}' if value is None else value}"]
    run = subprocess.run([pa, "factory", "compile", target, *args], capture_output=True, text=True,
                         stdin=subprocess.DEVNULL)
    if run.returncode:
        return None, (run.stderr or run.stdout).strip().removeprefix("Error: ")[-600:]
    try:
        return json.loads(run.stdout), None
    except ValueError as error:
        return None, str(error)


def describe(draft, meta):
    spec, error = graph(meta)
    return {"draft": str(draft), "file": meta["file"], "name": meta["name"], "target": meta["target"],
            "kind": meta["kind"], "project": meta.get("project"), "states": states(meta),
            "graph": spec, "graph_error": error}


def project_source(project, name):
    """machine.toml of a project factory, refusing a name or path that leaves the project."""
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,63}", name or ""):
        raise ValueError("Name a project factory with lowercase letters, digits, and dashes.")
    root = Path(project).resolve(strict=True) / ".prime" / "machines"
    source = (root / name / "machine.toml").resolve()
    if source.parent.parent != root.resolve() or not source.is_file():
        raise ValueError(f"No project factory {name} in {project}.")
    return source


def op_list(args):
    rows = []
    for name, row in machine_catalog(pa).items():
        path = Path(row["path"])
        rows.append({"name": name, "source": row["source"], "kind": "toml" if path.name == "machine.toml" else "md",
                     "editable": row["source"] == "user"})
    return {"machines": rows}


def op_draft(args):
    name = args.get("name") or ""
    if args.get("project"):
        return draft_from(project_source(args["project"], name), name, name, str(Path(args["project"]).resolve()))
    machines = machine_catalog(pa)
    if name not in machines:
        raise ValueError("Choose a library machine to edit.")
    machine = machines[name]
    source = Path(machine["path"])
    target = name
    if machine["source"] != "user":
        target = (args.get("target") or "my-" + name).strip()
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,63}", target) or target in machines:
            raise ValueError("Choose a new machine name. To change an existing copy, select it in the library.")
    return draft_from(source, name, target, None)


def draft_from(source, name, target, project):
    drafts_root().mkdir(parents=True, exist_ok=True)
    draft = Path(tempfile.mkdtemp(dir=drafts_root(), prefix=".pa-edit-"))
    try:
        if source.name == "machine.toml":
            copy = draft / target
            shutil.copytree(source.parent, copy)
            manifest = copy / source.name
            if target != name:
                manifest.write_text(re.sub(r"(?m)^name\s*=.*$", f'name = "{target}"', manifest.read_text(), count=1))
            meta = {"kind": "toml", "file": str(manifest), "dir": str(copy)}
        else:
            path = draft / "MACHINE.md"
            path.write_text(re.sub(r"(?m)^name:.*$", "name: " + target, source.read_text(), count=1))
            meta = {"kind": "md", "file": str(path)}
        meta.update(name=name, target=target, source=str(source), original=digest(source), project=project)
        meta_path(draft).write_text(json.dumps(meta))
    except BaseException:
        shutil.rmtree(draft, ignore_errors=True)
        raise
    return describe(draft, meta)


def op_states(args):
    draft, meta = load_draft(args)
    return describe(draft, meta)


def op_set_model(args):
    draft, meta = load_draft(args)
    if meta["kind"] != "md":
        raise ValueError("Press e to edit machine.toml and change a model.")
    path = Path(meta["file"])
    text = path.read_text()
    block, spec, rows = md_spec(text)
    state = next((s for s in rows if str(s["id"]) == args.get("state")), None)
    if state is None or not isinstance(state.get("subagent"), dict):
        raise ValueError("This state uses a referenced agent. Press e to edit the definition.")
    model = (args.get("model") or "").strip()
    if not model or model == "inherit":
        state["subagent"].pop("model", None)
    else:
        state["subagent"]["model"] = model
    path.write_text(text[:block.start(1)] + json.dumps(spec, ensure_ascii=False, indent=2) + "\n" + text[block.end(1):])
    return describe(draft, meta)


def op_read(args):
    _, meta = load_draft(args)
    return {"file": meta["file"], "text": Path(meta["file"]).read_text()}


def op_write(args):
    draft, meta = load_draft(args)
    text = args.get("text")
    if not isinstance(text, str) or not text.strip():
        raise ValueError("The definition is empty. The draft is unchanged.")
    Path(meta["file"]).write_text(text if text.endswith("\n") else text + "\n")
    return describe(draft, meta)


def op_save(args):
    draft, meta = load_draft(args)
    name, target, source = meta["name"], meta["target"], Path(meta["source"])
    machines = {} if meta.get("project") else machine_catalog(pa)
    changed = (target != name and target in machines) or (
        target == name and (not source.is_file() or digest(source) != meta["original"]))
    if changed:
        where = "project factory" if meta.get("project") else "library"
        raise ValueError(f"The {where} changed during this edit. The draft is kept at " + meta["file"])
    if meta["kind"] == "toml":
        checked = subprocess.run([pa, "factory", "validate", meta["dir"]], capture_output=True, text=True)
        if checked.returncode:
            raise ValueError("Validation failed. The installed machine is unchanged. "
                             + (checked.stderr or checked.stdout).strip()[-600:])
        manifest = Path(meta["file"])
        if tomllib.loads(manifest.read_text()).get("name") != target:
            raise ValueError(f"Keep the machine name {target}.")
        if target != name:
            destination = personal_machines() / target
            destination.parent.mkdir(parents=True, exist_ok=True)
            if destination.exists():
                raise ValueError(f"{destination} already exists.")
            Path(meta["dir"]).rename(destination)
        else:
            os.replace(manifest, source)
    else:
        text = Path(meta["file"]).read_text()
        front = text.split("---", 2)[1] if text.count("---") >= 2 else ""
        if not re.search(r"(?m)^name:\s*" + re.escape(target) + r"\s*$", front):
            raise ValueError("Keep the draft's machine name when saving.")
        imported = subprocess.run([pa, "factory", "import", meta["file"]], capture_output=True, text=True)
        if imported.returncode:
            raise ValueError("Validation failed. The installed machine is unchanged. "
                             + (imported.stderr or imported.stdout).strip()[-600:])
    shutil.rmtree(draft, ignore_errors=True)
    return {"saved": target}


def op_discard(args):
    draft, _ = load_draft(args)
    shutil.rmtree(draft, ignore_errors=True)
    return {"discarded": True}


OPS = {"list": op_list, "draft": op_draft, "states": op_states, "set-model": op_set_model,
       "read": op_read, "write": op_write, "save": op_save, "discard": op_discard}

try:
    op = sys.argv[2] if len(sys.argv) > 2 else ""
    if op not in OPS:
        raise ValueError("Use pa thread fsm-edit " + "|".join(OPS) + " with a JSON object on stdin.")
    raw = sys.stdin.read() if not sys.stdin.isatty() else ""
    print(json.dumps(OPS[op](json.loads(raw) if raw.strip() else {})))
except (KeyError, OSError, ValueError, subprocess.CalledProcessError) as error:
    sys.exit(f"Machine editor: {error}")
