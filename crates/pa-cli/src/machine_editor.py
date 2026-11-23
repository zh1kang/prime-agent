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
    """State rows for the draft: resolves [roles] in machine.toml and marks agent states editable."""
    path = Path(meta["file"])
    if meta["kind"] == "md":
        _, _, rows = md_spec(path.read_text())
        out = []
        for state in rows:
            agent = state.get("subagent")
            editable = isinstance(agent, dict)
            raw_model = str(agent.get("model") or "") if editable else ""
            thinking = str(agent.get("thinking") or "") if editable else ""
            label = (raw_model or "inherit") if editable else "see definition"
            if editable and thinking:
                label = f"{label} · {thinking}"
            out.append({
                "id": str(state["id"]),
                "kind": "python" if state.get("python") else "agent",
                "role": "",
                "model": label,
                "raw_model": raw_model,
                "thinking": thinking,
                "editable": editable,
            })
        return out
    try:
        manifest = tomllib.loads(path.read_text())
    except tomllib.TOMLDecodeError as error:
        return [{"id": "machine.toml", "kind": "error", "role": "", "model": "does not parse: " + str(error)[:120],
                 "raw_model": "", "thinking": "", "editable": False}]
    roles = manifest.get("roles") if isinstance(manifest.get("roles"), dict) else {}
    out = []
    for name, state in (manifest.get("states") or {}).items():
        if not isinstance(state, dict):
            continue
        agent = state.get("agent")
        if isinstance(agent, dict) and state.get("kind") != "python":
            role = str(agent.get("role") or "")
            role_cfg = roles.get(role) if role and isinstance(roles.get(role), dict) else {}
            raw_model = str(agent.get("model") or role_cfg.get("model") or "")
            thinking = str(agent.get("thinking") or role_cfg.get("thinking") or "")
            base = raw_model or "inherit"
            if role:
                label = f"{base} (role: {role})"
            else:
                label = base
            if thinking:
                label = f"{label} · {thinking}"
            out.append({
                "id": str(name),
                "kind": "agent",
                "role": role,
                "model": label,
                "raw_model": raw_model,
                "thinking": thinking,
                "editable": True,
            })
        else:
            kind = str(state.get("kind") or "python")
            out.append({
                "id": str(name),
                "kind": kind,
                "role": "",
                "model": kind,
                "raw_model": "",
                "thinking": "",
                "editable": False,
            })
    return out


def _toml_key(name):
    return name if re.fullmatch(r"[A-Za-z0-9_-]+", name) else json.dumps(name)


def _toml_str(value):
    return json.dumps(str(value), ensure_ascii=False)


def _split_inline_pairs(inner):
    """Split top-level `k = v` pairs inside a TOML inline table `{ ... }`."""
    parts = []
    cur = []
    quote = None
    escaped = False
    depth = 0
    for ch in inner:
        if quote:
            cur.append(ch)
            if escaped:
                escaped = False
            elif ch == "\\" and quote == '"':
                escaped = True
            elif ch == quote:
                quote = None
        else:
            if ch in ('"', "'"):
                quote = ch
                cur.append(ch)
            elif ch in ("{", "["):
                depth += 1
                cur.append(ch)
            elif ch in ("}", "]"):
                depth = max(0, depth - 1)
                cur.append(ch)
            elif ch == "," and depth == 0:
                piece = "".join(cur).strip()
                if piece:
                    parts.append(piece)
                cur = []
            else:
                cur.append(ch)
    piece = "".join(cur).strip()
    if piece:
        parts.append(piece)
    pairs = []
    for part in parts:
        if "=" in part:
            k, v = part.split("=", 1)
            pairs.append((k.strip(), v.strip()))
    return pairs


def _format_inline_table(pairs):
    return "{ " + ", ".join(f"{k} = {v}" for k, v in pairs) + " }"


def _string_end(line, start):
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


def _quoted_lines(lines):
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
                index = _string_end(line, index)
            else:
                index += 1
    return out


def _inline_end(line, start):
    """The index of the brace that closes the inline table at `line[start]`, or None."""
    depth = 0
    index = start
    while index < len(line):
        char = line[index]
        if char in "\"'":
            index = _string_end(line, index)
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


def _find_section(lines, targets):
    """Return `(start_idx, end_idx)` of the first TOML section header matching `targets`."""
    start = None
    quoted = _quoted_lines(lines)
    for idx, line in enumerate(lines):
        stripped = line.strip()
        if quoted[idx]:
            continue
        if stripped.startswith("[") and not stripped.startswith("[["):
            header = stripped.split("#", 1)[0].strip()
            if start is not None:
                return start, idx
            if header in targets:
                start = idx
    if start is not None:
        return start, len(lines)
    return None, None


def _update_section_kv(lines, start, end, updates, removals=()):
    """Update or remove top-level keys inside `[start+1, end)` of a TOML table."""
    body = lines[start + 1 : end]
    quoted = _quoted_lines(lines)[start + 1 : end]
    remaining = dict(updates)
    new_body = []
    for line, in_string in zip(body, quoted):
        stripped = line.strip()
        if in_string or not stripped or stripped.startswith("#") or "=" not in stripped:
            new_body.append(line)
            continue
        key = stripped.split("=", 1)[0].strip().strip('"').strip("'")
        if key in removals:
            continue
        if key in remaining:
            new_body.append(f"{_toml_key(key)} = {remaining.pop(key)}")
        else:
            new_body.append(line)
    # Insert any new keys before trailing blank lines.
    insert_at = len(new_body)
    while insert_at > 0 and not new_body[insert_at - 1].strip():
        insert_at -= 1
    for key, raw_val in remaining.items():
        new_body.insert(insert_at, f"{_toml_key(key)} = {raw_val}")
        insert_at += 1
    return lines[: start + 1] + new_body + lines[end:]


def edit_toml_role(text, role_name, model, thinking=""):
    """Update or create `[roles.<role_name>]` with `model` and optional `thinking`."""
    manifest = tomllib.loads(text)
    role_name = str(role_name or "").strip()
    if not role_name:
        raise ValueError("Specify a role name.")
    model = str(model or "").strip()
    if not model or model == "inherit":
        raise ValueError("A role requires a concrete model (cannot be empty or inherit).")
    thinking = str(thinking or "").strip()
    if thinking == "default":
        thinking = ""
    lines = text.splitlines()
    sub_targets = {f"[roles.{role_name}]", f'[roles."{role_name}"]'}
    s, e = _find_section(lines, sub_targets)
    if s is not None:
        updates = {"model": _toml_str(model)}
        removals = ()
        if thinking:
            updates["thinking"] = _toml_str(thinking)
        else:
            removals = ("thinking",)
        updated_lines = _update_section_kv(lines, s, e, updates, removals)
    else:
        pairs = [("model", _toml_str(model))]
        if thinking:
            pairs.append(("thinking", _toml_str(thinking)))
        entry = _format_inline_table(pairs)
        rs, re_idx = _find_section(lines, {"[roles]"})
        if rs is not None:
            updated_lines = _update_section_kv(lines, rs, re_idx, {role_name: entry})
        else:
            # Insert [roles] right before the first [states.*] header, or at EOF.
            insert_at = next(
                (i for i, l in enumerate(lines) if l.strip().startswith("[states.")),
                len(lines),
            )
            block = ["[roles]", f"{_toml_key(role_name)} = {entry}", ""]
            updated_lines = lines[:insert_at] + block + lines[insert_at:]
    result = "\n".join(updated_lines).rstrip("\n") + "\n"
    tomllib.loads(result)
    return result


def edit_toml_state_model(text, state_id, model=None, thinking="", role=None):
    """Set a state's agent model/thinking or role in `machine.toml`."""
    manifest = tomllib.loads(text)
    states_tbl = manifest.get("states") if isinstance(manifest.get("states"), dict) else {}
    state_id = str(state_id or "").strip()
    if state_id not in states_tbl or not isinstance(states_tbl[state_id], dict):
        raise ValueError(f"Unknown state {state_id}.")
    st = states_tbl[state_id]
    if st.get("kind") == "python" or not isinstance(st.get("agent"), dict):
        raise ValueError(f"State {state_id} is not an agent state.")
    thinking = str(thinking or "").strip()
    if thinking == "default":
        thinking = ""
    role = str(role or "").strip() if role is not None else ""
    if role:
        roles = manifest.get("roles") if isinstance(manifest.get("roles"), dict) else {}
        if role not in roles:
            raise ValueError(f"Unknown role {role}.")
    else:
        model = str(model or "").strip()
        if model == "inherit":
            model = ""

    lines = text.splitlines()
    sub_targets = {f"[states.{state_id}.agent]", f'[states."{state_id}".agent]'}
    s, e = _find_section(lines, sub_targets)
    if s is not None:
        if role:
            updates = {"role": _toml_str(role)}
            removals = ["model"]
            if thinking:
                updates["thinking"] = _toml_str(thinking)
            else:
                removals.append("thinking")
            updated_lines = _update_section_kv(lines, s, e, updates, tuple(removals))
        else:
            updates = {}
            removals = ["role"]
            if model:
                updates["model"] = _toml_str(model)
            else:
                removals.append("model")
            if thinking:
                updates["thinking"] = _toml_str(thinking)
            else:
                removals.append("thinking")
            updated_lines = _update_section_kv(lines, s, e, updates, tuple(removals))
    else:
        st_targets = {f"[states.{state_id}]", f'[states."{state_id}"]'}
        s, e = _find_section(lines, st_targets)
        if s is None:
            raise ValueError(f"Could not locate [states.{state_id}] in machine.toml.")
        found = False
        updated_lines = list(lines)
        quoted = _quoted_lines(lines)
        for idx in range(s + 1, e):
            stripped = lines[idx].strip()
            if not quoted[idx] and re.match(r"^agent\s*=\s*\{", stripped):
                open_b = lines[idx].index("{")
                close_b = _inline_end(lines[idx], open_b)
                if close_b is None:
                    break
                pairs = _split_inline_pairs(lines[idx][open_b + 1 : close_b])
                filtered = [
                    (k, v)
                    for k, v in pairs
                    if k.strip('"').strip("'") not in ("role", "model", "thinking")
                ]
                if role:
                    filtered.append(("role", _toml_str(role)))
                elif model:
                    filtered.append(("model", _toml_str(model)))
                if thinking:
                    filtered.append(("thinking", _toml_str(thinking)))
                updated_lines[idx] = lines[idx][:open_b] + _format_inline_table(filtered) + lines[idx][close_b + 1 :]
                found = True
                break
        if not found:
            raise ValueError(f"Could not locate agent table for state {state_id}.")
    result = "\n".join(updated_lines).rstrip("\n") + "\n"
    tomllib.loads(result)
    return result


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


NODE_FILE_CAP = 65536


def _safe_read_inside(root, rel_path):
    root = Path(root).resolve()
    target = (root / rel_path).resolve()
    if target != root and root not in target.parents:
        return None
    if not target.is_file():
        return None
    raw = target.read_bytes()
    return {
        "path": str(rel_path),
        "content": raw[:NODE_FILE_CAP].decode("utf-8", errors="replace"),
        "truncated": len(raw) > NODE_FILE_CAP,
    }


def draft_metadata(meta):
    """Description, params, roles, and per-state details + source files."""
    path = Path(meta["file"])
    if meta["kind"] != "toml":
        _, spec, rows = md_spec(path.read_text())
        details = {}
        for st in rows:
            sid = str(st["id"])
            agent = st.get("subagent") if isinstance(st.get("subagent"), dict) else {}
            files = []
            if isinstance(agent.get("prompt"), str) and agent["prompt"]:
                files.append({"label": "prompt (inline)", "path": "MACHINE.md", "content": agent["prompt"], "truncated": False})
            details[sid] = {
                "id": sid,
                "kind": "python" if st.get("python") else "agent",
                "entry": bool(st.get("entry")),
                "role": "",
                "model": str(agent.get("model") or ""),
                "thinking": str(agent.get("thinking") or ""),
                "run": "",
                "budget": str(st.get("budget") or ""),
                "max_entries": st.get("max_entries") or st.get("maxEntries"),
                "retries": st.get("retries"),
                "inputs": st.get("inputs") if isinstance(st.get("inputs"), dict) else {},
                "outputs": st.get("outputs") if isinstance(st.get("outputs"), dict) else {},
                "next": None,
                "route": None,
                "join": None,
                "files": files,
            }
        return str(spec.get("description") or ""), [], {}, details
    try:
        manifest = tomllib.loads(path.read_text())
    except tomllib.TOMLDecodeError:
        return "", [], {}, {}
    root = Path(meta["dir"])
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
    roles_raw = manifest.get("roles") if isinstance(manifest.get("roles"), dict) else {}
    roles = {
        str(rname): {
            "model": str(rcfg.get("model") or ""),
            "thinking": str(rcfg.get("thinking") or ""),
        }
        for rname, rcfg in roles_raw.items()
        if isinstance(rcfg, dict)
    }
    context_file = _safe_read_inside(root, "prompts/_context.md")
    details = {}
    for sid, st in (manifest.get("states") or {}).items():
        if not isinstance(st, dict):
            continue
        is_python = st.get("kind") == "python"
        agent = st.get("agent") if isinstance(st.get("agent"), dict) else {}
        role = agent.get("role") if not is_python and isinstance(agent.get("role"), str) else ""
        role_cfg = roles.get(role) or {}
        model = (agent.get("model") or role_cfg.get("model") or "") if not is_python else ""
        thinking = (agent.get("thinking") or role_cfg.get("thinking") or "") if not is_python else ""
        files = []
        if is_python:
            run_spec = str(st.get("run") or "")
            py_rel = run_spec.split(":", 1)[0]
            if py_rel:
                loaded = _safe_read_inside(root, py_rel)
                if loaded:
                    files.append({"label": f"python ({run_spec})", **loaded})
        else:
            prompt_rel = agent.get("prompt")
            if isinstance(prompt_rel, str) and prompt_rel:
                loaded = _safe_read_inside(root, prompt_rel)
                if loaded:
                    files.append({"label": f"prompt ({prompt_rel})", **loaded})
            rubric_rel = agent.get("rubric")
            if isinstance(rubric_rel, str) and rubric_rel:
                loaded = _safe_read_inside(root, rubric_rel)
                if loaded:
                    files.append({"label": f"rubric ({rubric_rel})", **loaded})
            if context_file:
                files.append({"label": "shared context (prompts/_context.md)", **context_file})
        details[str(sid)] = {
            "id": str(sid),
            "kind": "python" if is_python else "agent",
            "entry": bool(st.get("entry")),
            "role": role,
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
    return description, params, roles, details


def describe(draft, meta):
    spec, error = graph(meta)
    description, params, roles, state_details = draft_metadata(meta)
    return {"draft": str(draft), "file": meta["file"], "name": meta["name"], "target": meta["target"],
            "kind": meta["kind"], "project": meta.get("project"), "description": description,
            "params": params, "roles": roles, "states": states(meta), "state_details": state_details,
            "graph": spec, "graph_error": error}


def project_source(project, name):
    """machine.toml of a project factory, refusing a name or path that leaves the project."""
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,63}", name or ""):
        raise ValueError("Name a project factory with lowercase letters, digits, and dashes.")
    project = Path(project).resolve(strict=True)
    root = project / ".prime" / "machines"
    for part in (project / ".prime", root, root / name, root / name / "machine.toml"):
        if part.is_symlink() or (part.exists() and not part.resolve().is_relative_to(project)):
            raise ValueError(f"{part} is a symlink or leaves the project folder.")
    source = root / name / "machine.toml"
    if not source.is_file():
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
            refuse_escaping_links(source.parent)
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


def refuse_escaping_links(directory):
    """Refuse a machine folder with a symlink whose target leaves it, as the machine loader does."""
    root = Path(directory).resolve()
    for current, folders, files in os.walk(directory):
        for entry in folders + files:
            path = Path(current) / entry
            if path.is_symlink() and not path.resolve().is_relative_to(root):
                raise ValueError(f"{path}: the symlink target is outside {root}.")


def op_states(args):
    draft, meta = load_draft(args)
    return describe(draft, meta)


def op_set_model(args):
    draft, meta = load_draft(args)
    path = Path(meta["file"])
    text = path.read_text()
    if meta["kind"] == "toml":
        if args.get("edit_role"):
            target_role = str(args.get("role") or "").strip()
            if not target_role and args.get("state"):
                manifest = tomllib.loads(text)
                st = (manifest.get("states") or {}).get(str(args["state"]).strip())
                if isinstance(st, dict) and isinstance(st.get("agent"), dict):
                    target_role = str(st["agent"].get("role") or "").strip()
            if not target_role:
                raise ValueError(f"State {args.get('state') or ''} does not use a role.")
            updated = edit_toml_role(
                text,
                target_role,
                args.get("model"),
                args.get("thinking") or "",
            )
        elif args.get("role"):
            updated = edit_toml_state_model(
                text,
                args.get("state"),
                role=args.get("role"),
                thinking=args.get("thinking") or "",
            )
        else:
            updated = edit_toml_state_model(
                text,
                args.get("state"),
                model=args.get("model"),
                thinking=args.get("thinking") or "",
            )
        path.write_text(updated)
        return describe(draft, meta)
    block, spec, rows = md_spec(text)
    state = next((s for s in rows if str(s["id"]) == args.get("state")), None)
    if state is None or not isinstance(state.get("subagent"), dict):
        raise ValueError("This state uses a referenced agent. Press e to edit the definition.")
    model = (args.get("model") or "").strip()
    thinking = (args.get("thinking") or "").strip()
    if thinking == "default":
        thinking = ""
    if not model or model == "inherit":
        state["subagent"].pop("model", None)
    else:
        state["subagent"]["model"] = model
    if thinking:
        state["subagent"]["thinking"] = thinking
    elif "thinking" in args:
        state["subagent"].pop("thinking", None)
    path.write_text(text[:block.start(1)] + json.dumps(spec, ensure_ascii=False, indent=2) + "\n" + text[block.end(1):])
    return describe(draft, meta)


def op_set_role_model(args):
    draft, meta = load_draft(args)
    if meta["kind"] != "toml":
        raise ValueError("Roles apply to machine.toml factories.")
    path = Path(meta["file"])
    updated = edit_toml_role(
        path.read_text(),
        args.get("role"),
        args.get("model"),
        args.get("thinking") or "",
    )
    path.write_text(updated)
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
    if meta.get("project"):
        source = project_source(meta["project"], name)  # checked again: the folder may have changed
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
            # The draft folder can be on another filesystem: stage the file beside the target.
            staged = tempfile.NamedTemporaryFile(dir=source.parent, prefix=".machine.toml-", delete=False)
            try:
                with staged:
                    staged.write(manifest.read_bytes())
                os.chmod(staged.name, source.stat().st_mode & 0o777)
                os.replace(staged.name, source)
            except BaseException:
                Path(staged.name).unlink(missing_ok=True)
                raise
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
       "set-role-model": op_set_role_model, "read": op_read, "write": op_write,
       "save": op_save, "discard": op_discard}

try:
    op = sys.argv[2] if len(sys.argv) > 2 else ""
    if op not in OPS:
        raise ValueError("Use pa thread fsm-edit " + "|".join(OPS) + " with a JSON object on stdin.")
    raw = sys.stdin.read() if not sys.stdin.isatty() else ""
    print(json.dumps(OPS[op](json.loads(raw) if raw.strip() else {})))
except (KeyError, OSError, ValueError, subprocess.CalledProcessError) as error:
    sys.exit(f"Machine editor: {error}")
