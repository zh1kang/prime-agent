import json
import os
from pathlib import Path
import subprocess


def personal_machines(agent=None):
    agent = agent or os.getenv("PRIME_AGENT_CODING_AGENT_DIR") or os.getenv("PI_CODING_AGENT_DIR")
    return (Path(agent).expanduser() if agent else Path.home() / ".prime/agent") / "machines"


def machine_catalog(pa, agent=None):
    """Installed machines by name. `agent` selects the agent home that owns them."""
    env = {**os.environ, "PRIME_AGENT_CODING_AGENT_DIR": str(agent)} if agent else None
    reply = subprocess.check_output([pa, "factory", "list", "--json"], text=True, env=env)
    entries = json.loads(reply)["machines"]
    roots = {row["source"]: Path(row["path"]).parent.parent for row in entries}
    roots.setdefault("user", personal_machines(agent))
    if os.getenv("PRIME_AGENT_MACHINES_DIR"):
        roots["repo"] = Path(os.environ["PRIME_AGENT_MACHINES_DIR"])
    machines = {}
    for source in ("user", "repo"):
        for row in entries:
            if row["source"] == source:
                machines[row["name"]] = row
        if source in roots:
            for definition in roots[source].glob("*/machine.toml"):
                name = definition.parent.name
                machines[name] = {"name": name, "source": source, "path": str(definition)}
    return dict(sorted(machines.items()))



def machine_access(row):
    """'read' or 'write' from the machine's captain.toml; unknown machines write.

    A project factory always writes: the Captain writes its files, captain.toml included.
    """
    import tomllib

    if row.get("source") == "project":
        return "write"
    try:
        with (Path(row["path"]).parent / "captain.toml").open("rb") as stream:
            access = tomllib.load(stream).get("access")
    except (OSError, ValueError):
        return "write"
    return access if access in ("read", "write") else "write"


def machine_params(row):
    """The declared params of a machine directory; MACHINE.md machines take none."""
    import tomllib

    path = Path(row["path"])
    if path.name != "machine.toml":
        return {}
    with path.open("rb") as stream:
        return tomllib.load(stream).get("params", {})


def checked_machine_params(row, params):
    """Validate params against the machine's declared fields at the dispatch boundary."""
    if params is None:
        params = {}
    if not isinstance(params, dict):
        raise ValueError("Pass params as a JSON object.")
    fields = machine_params(row)
    unknown = sorted(set(params) - set(fields))
    if unknown:
        raise ValueError(
            f"{row['name']} has no parameter {', '.join(unknown)}."
            + (f" Parameters: {', '.join(fields)}." if fields else " It takes no parameters.")
        )
    missing = [name for name, spec in fields.items() if "default" not in spec and name not in params]
    if missing:
        raise ValueError(f"{row['name']} requires {', '.join(missing)}.")
    for name, value in params.items():
        if fields[name].get("type") == "text" and (not isinstance(value, str) or not value.strip()):
            raise ValueError(f"Parameter {name} must be non-empty text.")
    return params


def machine_purpose(row):
    """One short line from the catalog description."""
    text = " ".join(str(row.get("description") or "").split())
    if not text:
        try:
            if Path(row["path"]).name == "machine.toml":
                import tomllib

                with Path(row["path"]).open("rb") as stream:
                    text = " ".join(str(tomllib.load(stream).get("description", "")).split())
        except (OSError, ValueError):
            text = ""
    return text[:120] or "No description."
