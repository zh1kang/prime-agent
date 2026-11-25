"""Live quota and tier brief for the Captain's lead prompt.

The router implementation lives in the runtime. This helper loads it by
path when the source tree is next to the ``pa`` binary, and otherwise reads
the quota cache the runtime already wrote. A failure here must not block a
Captain turn.
"""

import importlib.util
import sys


_ROUTER_MOD = None
_ROUTER_LOOKED = False


def _router_source():
    explicit = os.environ.get("PA_MODEL_ROUTER_PY")
    if explicit and Path(explicit).is_file():
        return Path(explicit)
    direct_candidates = []
    try:
        pa_dir = Path(PA).resolve().parent
        direct_candidates.extend(
            [
                pa_dir / "prime-agent-runtime" / "src" / "rlm" / "model_router.py",
                pa_dir / "rlm" / "model_router.py",
            ]
        )
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
    sys.modules[spec.name] = module
    try:
        spec.loader.exec_module(module)
    except Exception:
        sys.modules.pop(spec.name, None)
        raise
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
        row = db.execute("SELECT * FROM projects WHERE id=?", (project_id,)).fetchone()
        if row is not None and "project_usage_detail" in globals():
            detail = project_usage_detail(db, dict(row))
            totals = detail.get("usage") if isinstance(detail, dict) else None
            if isinstance(totals, dict):
                usage = totals
    except Exception:
        usage = None
    if usage is None:
        try:
            usage = job_usage(db, "project", project_id)
        except Exception:
            usage = None
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


_FALLBACK_CLI_BANNER_MARKERS = (
    "invalid api key",
    "fix external api key",
    "authentication_error",
    "no api key found",
    "not logged in",
    "please run /login",
    "individual quota reached",
    "exceeded your current quota",
    "please upgrade your subscription",
    "you've hit your limit",
    "you have hit your limit",
    "out of extra usage",
    "credit balance is too low",
    "rate_limit_error",
    "overloaded_error",
)


def captain_is_provider_error(text):
    """True when ``text`` describes a provider quota, rate-limit, or auth failure."""
    if not isinstance(text, str) or not text.strip():
        return False
    try:
        module = _router_module()
        if module is not None and hasattr(module, "is_provider_error_text"):
            return bool(module.is_provider_error_text(text))
    except Exception:
        pass
    lowered = text.lower()
    return any(marker in lowered for marker in _FALLBACK_CLI_BANNER_MARKERS)


def captain_is_cli_banner_error(text):
    """True only when ``text`` matches an unmistakable provider CLI auth/quota banner."""
    if not isinstance(text, str) or not text.strip():
        return False
    try:
        module = _router_module()
        if module is not None and hasattr(module, "is_cli_banner_error_text"):
            return bool(module.is_cli_banner_error_text(text))
    except Exception:
        pass
    lowered = text.lower()
    return any(marker in lowered for marker in _FALLBACK_CLI_BANNER_MARKERS)


def captain_default_agent_model():
    """Return ``provider/model`` from the agent settings file, or empty string."""
    roots = []
    if "NATIVE" in globals():
        try:
            roots.append(Path(NATIVE))
        except Exception:
            pass
    env_dir = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
    if env_dir:
        roots.append(Path(env_dir))
    roots.append(Path.home() / ".prime" / "agent")
    for root in roots:
        try:
            data = json.loads((root / "settings.json").read_text(encoding="utf-8"))
        except (OSError, ValueError, TypeError):
            continue
        if isinstance(data, dict):
            provider = data.get("defaultProvider")
            model = data.get("defaultModel")
            if isinstance(provider, str) and provider.strip() and isinstance(model, str) and model.strip():
                if "/" in model:
                    return model.strip()
                return f"{provider.strip()}/{model.strip()}"
    return ""


def captain_session_quota_error(session_file, offset=0):
    """Scan newly written bytes of a native session JSONL for a quota/auth error."""
    if not session_file:
        return None
    try:
        module = _router_module()
        if module is not None and hasattr(module, "scan_session_jsonl_quota_error"):
            return module.scan_session_jsonl_quota_error(session_file, offset)
    except Exception:
        pass
    path = Path(session_file)
    try:
        size = path.stat().st_size
    except OSError:
        return None
    start_offset = max(0, int(offset or 0))
    if size <= start_offset:
        return None
    start = max(start_offset, size - 32768)
    try:
        with path.open("rb") as fh:
            fh.seek(start)
            chunk = fh.read().decode("utf-8", errors="replace")
    except OSError:
        return None
    lines = [line for line in chunk.splitlines() if line.strip()][-40:]
    for raw in reversed(lines):
        try:
            row = json.loads(raw)
        except ValueError:
            continue
        if not isinstance(row, dict):
            continue
        if row.get("type") == "custom_message" and row.get("customType") == "provider_retry_outcome":
            details = row.get("details")
            if not isinstance(details, dict) or details.get("success") is not False:
                continue
            final_err = details.get("finalError")
            msg = final_err if isinstance(final_err, str) and final_err.strip() else row.get("content")
            if isinstance(msg, str) and captain_is_provider_error(msg):
                return " ".join(msg.split())[:220]
        if row.get("type") == "message":
            msg = row.get("message")
            if not isinstance(msg, dict):
                continue
            err_msg = msg.get("errorMessage")
            if isinstance(err_msg, str) and captain_is_provider_error(err_msg):
                return " ".join(err_msg.split())[:220]
    return None


def captain_route_model(model, thinking="", *, prompt="", role=None, state_id="", attempt=1):
    """Pre-route a Captain/crew/factory model if its pool is exhausted or Active steering applies.

    Returns ``(chosen_model, chosen_thinking, note_or_None)``.
    """
    if not isinstance(model, str) or not model.strip():
        return model or "", thinking or "", None
    try:
        module = _router_module()
    except Exception:
        module = None
    if module is None:
        return ("" if model.strip().lower() == "router" else model), thinking or "", None
    if model.strip().lower() == "router":
        try:
            decision = (
                module.routed_step_decision(
                    prompt=prompt or "",
                    state_id=state_id or "",
                    role=role,
                    specialist=None,
                    attempt=max(1, int(attempt)),
                )
                if hasattr(module, "routed_step_decision")
                else None
            )
        except Exception:
            decision = None
        if decision is None or decision.exhausted or not decision.model:
            return "", thinking or "", None
        chosen_thinking = decision.thinking or thinking or ""
        return decision.model, chosen_thinking, f"router -> {decision.model} ({decision.reason})"
    try:
        decision = module.admission_decision(
            prompt=prompt or "",
            state_id=state_id or "",
            role=role,
            specialist=None,
            attempt=max(1, int(attempt)),
            pinned_model=model,
            pinned_thinking=thinking or None,
        )
    except Exception:
        return model, thinking or "", None
    if decision is None or decision.exhausted or not decision.model:
        return model, thinking or "", None
    chosen_thinking = decision.thinking or thinking or ""
    if decision.model != model:
        note = f"{model} -> {decision.model} ({decision.reason})"
        return decision.model, chosen_thinking, note
    return model, chosen_thinking, None


def captain_note_success(model, latency_ms):
    """Record a clean Captain or crew turn on ``model``'s pool. Never blocks the turn."""
    if not isinstance(model, str) or not model.strip():
        return
    try:
        module = _router_module()
        if module is not None:
            module.note_provider_success(model, latency_ms)
    except Exception:
        pass


def captain_record_and_fallback(
    model,
    thinking="",
    detail="",
    *,
    prompt="",
    role=None,
    state_id="",
    attempt=1,
    exclude=(),
):
    """Record a quota/auth failure on ``model`` and pick the next healthy fallback.

    Returns ``(fallback_model, fallback_thinking, note_or_None, exhausted_reason_or_None)``.
    """
    if not isinstance(model, str) or not model.strip():
        return "", thinking or "", None, None
    try:
        module = _router_module()
    except Exception:
        module = None
    if module is None:
        return "", thinking or "", None, None
    try:
        if module.router_disabled() or not module.is_recognized_model(model):
            return "", thinking or "", None, None
        module.note_provider_failure(model, detail or "quota exceeded")
        decision = module.fallback_after_failure(
            prompt=prompt or "",
            state_id=state_id or "",
            role=role,
            specialist=None,
            attempt=max(1, int(attempt)),
            model=model,
            thinking=thinking or None,
            exclude_models=tuple(exclude),
        )
    except Exception:
        return "", thinking or "", None, None
    if decision.model and not decision.exhausted and decision.model != model:
        note = f"{model} -> {decision.model} ({decision.reason})"
        return decision.model, decision.thinking or thinking or "", note, None
    exhausted = decision.reason.strip() if decision.exhausted and decision.reason else None
    return "", thinking or "", None, exhausted
