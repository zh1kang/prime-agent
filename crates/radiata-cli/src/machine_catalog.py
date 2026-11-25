import functools
import json
import os
from pathlib import Path
import subprocess


def personal_machines(agent=None):
    agent = agent or os.getenv("PRIME_AGENT_CODING_AGENT_DIR") or os.getenv("PI_CODING_AGENT_DIR")
    return (Path(agent).expanduser() if agent else Path.home() / ".prime/agent") / "machines"


FACTORY_LISTING_CACHE = "factory-listing-cache.json"
MACHINE_FILES = ("*/machine.toml", "*/captain.toml", "*/MACHINE.md")


def _mtime_ns(path):
    try:
        return path.stat().st_mtime_ns
    except OSError:
        return None


def _listing_stamp(pa, agent, levels):
    """What a factory listing depends on: pa (and its mtime), the agent home, and the mtimes
    of every machine directory and machine file it read. Upgrading pa or adding, removing, or
    editing a machine changes it."""
    roots = {*levels, str(personal_machines(agent))}
    if os.getenv("PRIME_AGENT_MACHINES_DIR"):
        roots.add(os.environ["PRIME_AGENT_MACHINES_DIR"])
    files = []
    for root in sorted(roots):
        folder = Path(root)
        files.append([root, _mtime_ns(folder)])
        for pattern in MACHINE_FILES:
            files.extend([str(path), _mtime_ns(path)] for path in folder.glob(pattern))
    return [str(pa), _mtime_ns(Path(pa)), os.getenv("PRIME_AGENT_MACHINES_DIR") or "", sorted(files)]


def _listing_levels(listing):
    rows = listing.get("levels") if isinstance(listing, dict) else None
    return sorted(row["path"] for row in rows or () if isinstance(row, dict) and isinstance(row.get("path"), str))


_LISTING_MEM = {}  # (pa, agent) -> (stamp, levels, listing_text)


def _factory_listing_text(pa, agent):
    """The raw `pa factory list --json` reply, cached on disk in the agent home under
    captains/ and reused while _listing_stamp is unchanged."""
    mem_key = (str(pa), str(agent) if agent is not None else None)
    mem = _LISTING_MEM.get(mem_key)
    if mem is not None:
        m_stamp, m_levels, m_text = mem
        if m_stamp == _listing_stamp(pa, agent, m_levels):
            return m_text
    cache = personal_machines(agent).parent / "captains" / FACTORY_LISTING_CACHE
    try:
        cached = json.loads(cache.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        cached = None
    levels = []
    if isinstance(cached, dict) and isinstance(cached.get("listing"), str):
        try:
            levels = _listing_levels(json.loads(cached["listing"]))
        except ValueError:
            levels = []
        current_stamp = _listing_stamp(pa, agent, levels)
        if cached.get("stamp") == current_stamp:
            _LISTING_MEM[mem_key] = (current_stamp, levels, cached["listing"])
            return cached["listing"]
    # The stamp is taken before the listing, so a machine edited meanwhile changes it again.
    stamp = _listing_stamp(pa, agent, levels)
    env = {**os.environ, "PRIME_AGENT_CODING_AGENT_DIR": str(agent)} if agent else None
    text = subprocess.check_output([pa, "factory", "list", "--json"], text=True, env=env)
    listed = _listing_levels(json.loads(text))
    if listed != levels:
        stamp = _listing_stamp(pa, agent, listed)
    _LISTING_MEM[mem_key] = (stamp, listed, text)
    try:
        cache.parent.mkdir(parents=True, exist_ok=True)
        temporary = cache.with_name(f".{cache.name}.{os.getpid()}.tmp")
        temporary.write_text(json.dumps({"stamp": stamp, "listing": text}), encoding="utf-8")
        os.replace(temporary, cache)
    except OSError:
        pass
    return text


def factory_listing(pa, agent=None):
    """The `pa factory list --json` reply. `agent` selects the agent home."""
    return json.loads(_factory_listing_text(pa, agent))


@functools.lru_cache(maxsize=1)
def bundled_modules_dir(pa):
    """The factory module library that the runtime package ships; it does not move while pa runs."""
    value = factory_listing(pa).get("modules_dir")
    return Path(value) if isinstance(value, str) and value else None


def machine_catalog(pa, agent=None):
    """Installed machines by name. `agent` selects the agent home that owns them."""
    entries = factory_listing(pa, agent)["machines"]
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
