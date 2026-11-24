"""Live quota and tier brief for the Captain's lead prompt.

The router implementation lives in the runtime. This helper loads it by
path when the source tree is next to the ``pa`` binary, and otherwise reads
the quota cache the runtime already wrote. A failure here must not block a
Captain turn.
"""

import importlib.util


_ROUTER_MOD = None
_ROUTER_LOOKED = False


def _router_source():
    explicit = os.environ.get("PA_MODEL_ROUTER_PY")
    if explicit and Path(explicit).is_file():
        return Path(explicit)
    # Check installed pa-harness runtime and kernel-venv site-packages first.
    direct_candidates = [
        Path.home()
        / ".local"
        / "share"
        / "pa-harness"
        / "prime-agent-runtime"
        / "src"
        / "rlm"
        / "model_router.py",
        Path.home() / ".local" / "share" / "pa-harness" / "rlm" / "model_router.py",
    ]
    try:
        pa_dir = Path(PA).resolve().parent
        direct_candidates.insert(
            0, pa_dir / "prime-agent-runtime" / "src" / "rlm" / "model_router.py"
        )
        direct_candidates.insert(1, pa_dir / "rlm" / "model_router.py")
    except (NameError, OSError):
        pass
    for candidate in direct_candidates:
        if candidate.is_file():
            return candidate
    try:
        spec = importlib.util.find_spec("rlm.model_router")
        if spec and spec.origin and Path(spec.origin).is_file():
            return Path(spec.origin)
    except (ImportError, ValueError, OSError):
        pass
    roots = [Path.cwd()]
    try:
        roots.extend(Path(PA).resolve().parents)
    except (NameError, OSError):
        pass
    seen = set()
    for root in roots:
        if root in seen:
            continue
        seen.add(root)
        candidate = root / "prime-agent-runtime" / "src" / "rlm" / "model_router.py"
        if candidate.is_file():
            return candidate
        if len(seen) > 8:
            break
    return None


def _router_module():
    global _ROUTER_MOD, _ROUTER_LOOKED
    if _ROUTER_LOOKED:
        return _ROUTER_MOD
    _ROUTER_LOOKED = True
    path = _router_source()
    if path is None:
        return None
    spec = importlib.util.spec_from_file_location("pa_model_router", path)
    if spec is None or spec.loader is None:
        return None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    _ROUTER_MOD = module
    return module


def _fallback_quota_line():
    """One line from the on-disk quota cache when the runtime module is absent."""
    try:
        cache = HUB / "router-quota-cache.json"
        document = json.loads(cache.read_text(encoding="utf-8"))
    except (NameError, OSError, ValueError, TypeError):
        return ""
    raw = document.get("raw") if isinstance(document, dict) else None
    if not isinstance(raw, str) or not raw.strip():
        return ""
    try:
        payload = json.loads(raw)
    except ValueError:
        return ""
    rows = payload if isinstance(payload, list) else [payload]
    bits = []
    for entry in rows:
        if not isinstance(entry, dict) or not isinstance(entry.get("provider"), str):
            continue
        usage = entry.get("usage") if isinstance(entry.get("usage"), dict) else {}
        percents = []
        for slot in ("primary", "secondary", "tertiary"):
            window = usage.get(slot)
            if isinstance(window, dict) and isinstance(window.get("usedPercent"), (int, float)):
                percents.append(f"{slot} {float(window['usedPercent']):.0f}%")
        if percents:
            bits.append(entry["provider"] + " " + "/".join(percents))
    if not bits:
        return ""
    return "Pools: " + "; ".join(bits) + "."


def captain_router_brief(db, project_id):
    """Quota headroom, project spend, and the model to prefer at each tier."""
    usage = None
    try:
        usage = job_usage(db, "project", project_id)
    except Exception:
        usage = None
    # Prefer true project-wide cost across all sessions + subagents when available.
    try:
        row = db.execute("SELECT * FROM projects WHERE id=?", (project_id,)).fetchone()
        if row is not None and "project_usage_detail" in globals():
            detail = project_usage_detail(row)
            if isinstance(detail, dict) and detail.get("cost_usd") is not None:
                usage = dict(usage or {})
                usage["cost_usd"] = max(float(usage.get("cost_usd") or 0.0), float(detail["cost_usd"]))
    except Exception:
        pass
    module = None
    try:
        module = _router_module()
    except Exception:
        module = None
    if module is not None:
        kwargs = {}
        if isinstance(usage, dict):
            kwargs = {
                "spend_usd": usage.get("cost_usd") or 0.0,
                "input_tokens": usage.get("input_tokens") or 0,
                "output_tokens": usage.get("output_tokens") or 0,
            }
        body = module.format_captain_router_brief(**kwargs).strip()
    else:
        body = _fallback_quota_line()
        if isinstance(usage, dict):
            spend = "Project spend: $%.2f (in %s, out %s)." % (
                float(usage.get("cost_usd") or 0.0),
                int(usage.get("input_tokens") or 0),
                int(usage.get("output_tokens") or 0),
            )
            body = (body + "\n" + spend).strip() if body else spend
    if not body:
        return ""
    return "Live model routing:\n" + body + "\n"
