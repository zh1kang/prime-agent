"""Quota-, cost-, and complexity-aware model router.

Picks a model tier from the task, then a concrete model whose provider still
has quota headroom and a closed circuit. Live quota comes from
``codexbar usage --json-only`` through a short-timeout cache so a spawn never
waits on a stuck CLI. The catalog and the CLI checks mirror the models the
factory and Captain can actually launch.
"""

from __future__ import annotations

import fcntl
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Mapping

TIERS: tuple[str, ...] = ("fast", "balanced", "strong", "frontier")
TIER_RANK: dict[str, int] = {tier: index for index, tier in enumerate(TIERS)}
TIER_THINKING: dict[str, str] = {
    "fast": "low",
    "balanced": "medium",
    "strong": "high",
    "frontier": "high",
}

SOFT_THRESHOLD = 90.0
HARD_THRESHOLD = 98.0
QUOTA_CACHE_TTL_SECONDS = 60.0
QUOTA_FETCH_TIMEOUT_SECONDS = 15.0
EWMA_ALPHA = 0.2
RATE_LIMIT_COOLDOWN_SECONDS = 60.0
QUOTA_COOLDOWN_SECONDS = 15 * 60.0
AUTH_COOLDOWN_SECONDS = 30 * 60.0

CACHE_NAME = "router-quota-cache.json"
HEALTH_NAME = "router-health.json"
HEALTH_LOCK_NAME = "router-health.lock"
PROFILE_NAME = "router-profile.json"

PROVIDER_PRIORITIES: tuple[str, ...] = (
    "quota_aware",
    "cost_only",
    "favor_cost",
    "favor_throughput",
    "throughput_only",
)
SWITCHYARD_SENSITIVITIES: tuple[str, ...] = ("frugal", "balanced", "quality")
# Ramp Router's safe auto-compact cap below 922K OpenAI 1M-class hard limit.
SAFE_CROSS_PROVIDER_CONTEXT_CAP = 900_000

_OFF = frozenset({"0", "false", "off", "no"})
_ON = frozenset({"1", "true", "on", "yes"})
_RECOGNIZED_PREFIXES = (
    "anthropic/",
    "openai-codex/",
    "antigravity/",
    "antigravity-cli/",
    "cursor/",
    "cursor-cli/",
    "claude-cli/",
    "codex/",
)
_PROVIDER_POOLS: dict[str, tuple[str, ...]] = {
    "claude": ("claude",),
    "codex": ("codex",),
    "antigravity": ("antigravity-gemini", "antigravity-claude-gpt"),
    "cursor": ("cursor-total", "cursor", "cursor-third-party"),
}
_FRONTIER_PHRASES = (
    "architecture",
    "architect",
    "system design",
    "security audit",
    "threat model",
    "methodology",
    "deep math",
    "formal proof",
    "cryptograph",
    "root cause",
    "root-cause",
    "hard debug",
)
_FRONTIER_WORDS = ("algorithm", "proof", "auditor")
_STRONG_PHRASES = (
    "code review",
    "race condition",
    "incident",
    "migration",
    "concurrency",
    "performance",
)
_STRONG_WORDS = ("debug", "refactor", "audit", "review")
_FAST_PHRASES = (
    "formatting",
    "documentation",
    "changelog",
    "summarize",
    "summary",
    "spelling",
    "whitespace",
    "boilerplate",
    "readme",
)
_FAST_WORDS = ("typo", "format", "docs", "rename", "comment", "lint", "chore", "nit")
_ROLE_TIERS = {
    "architect": "frontier",
    "architecture": "frontier",
    "captain": "frontier",
    "lead": "frontier",
    "security": "frontier",
    "auditor": "frontier",
    "audit": "frontier",
    "researcher": "strong",
    "reviewer": "strong",
    "review": "strong",
    "implementer": "balanced",
    "coder": "balanced",
    "editor": "fast",
    "docs": "fast",
    "formatter": "fast",
    "scribe": "fast",
}
_STATE_TIERS = (
    ("frontier", ("audit", "architect", "security")),
    ("strong", ("review", "debug", "migrate")),
    ("fast", ("doc", "format", "summary", "lint", "chore")),
    ("balanced", ("implement", "build", "test")),
)


@dataclass(frozen=True)
class ModelCandidate:
    """One launchable model at one tier, billed against one quota pool."""

    model: str
    tier: str
    pool: str
    provider: str
    cost: float
    thinking: str


# Relative output prices keep the ladder frugal inside a tier. The same model
# can sit at more than one tier when only the thinking effort changes.
CATALOG: tuple[ModelCandidate, ...] = (
    ModelCandidate("anthropic/claude-opus-5-5", "frontier", "claude", "claude", 25.0, "high"),
    ModelCandidate("openai-codex/gpt-5.5", "frontier", "codex", "codex", 15.0, "high"),
    ModelCandidate("openai-codex/gpt-6.1-sol", "frontier", "codex", "codex", 16.0, "high"),
    ModelCandidate("antigravity/gemini-4-neon", "frontier", "antigravity-gemini", "antigravity", 15.0, "high"),
    ModelCandidate("antigravity/claude-opus-5-5", "frontier", "antigravity-claude-gpt", "antigravity", 25.0, "high"),
    ModelCandidate("cursor/claude-opus-5-5", "frontier", "cursor-third-party", "cursor", 25.0, "high"),
    ModelCandidate("anthropic/claude-sonnet-5-5", "strong", "claude", "claude", 15.0, "high"),
    ModelCandidate("openai-codex/gpt-5.3-codex", "strong", "codex", "codex", 15.0, "high"),
    ModelCandidate("antigravity/gemini-4-neon", "strong", "antigravity-gemini", "antigravity", 15.0, "high"),
    ModelCandidate("antigravity/claude-sonnet-5-5", "strong", "antigravity-claude-gpt", "antigravity", 15.0, "high"),
    ModelCandidate("cursor/gpt-5.5", "strong", "cursor-third-party", "cursor", 15.0, "high"),
    ModelCandidate("cursor/grok-4.7", "strong", "cursor-third-party", "cursor", 12.0, "high"),
    ModelCandidate("anthropic/claude-sonnet-5-5", "balanced", "claude", "claude", 15.0, "medium"),
    ModelCandidate("openai-codex/gpt-5.3-codex", "balanced", "codex", "codex", 15.0, "medium"),
    ModelCandidate("openai-codex/gpt-6-luna", "balanced", "codex", "codex", 8.0, "medium"),
    ModelCandidate("antigravity/gemini-4-argon", "balanced", "antigravity-gemini", "antigravity", 10.0, "medium"),
    ModelCandidate("antigravity/gpt-oss-120b", "balanced", "antigravity-claude-gpt", "antigravity", 8.0, "medium"),
    ModelCandidate("cursor/composer-2.5", "balanced", "cursor", "cursor", 6.0, "medium"),
    ModelCandidate("anthropic/claude-haiku-4-5", "fast", "claude", "claude", 5.0, "low"),
    ModelCandidate("openai-codex/gpt-5.4-mini", "fast", "codex", "codex", 2.0, "low"),
    ModelCandidate("antigravity/gemini-3.8-flash", "fast", "antigravity-gemini", "antigravity", 3.0, "low"),
    ModelCandidate("cursor/composer-2.5-fast", "fast", "cursor", "cursor", 6.0, "low"),
)


@dataclass(frozen=True)
class SwitchyardConfig:
    """Ramp Router V2 Switchyard 2-tier pair (efficient vs capable)."""

    enabled: bool = False
    efficient_model: str = ""
    capable_model: str = ""
    sensitivity: str = "balanced"


@dataclass(frozen=True)
class RoutingProfile:
    """Ramp Router V2-compatible workload routing profile."""

    slug: str = "coding-agents"
    label: str = "Coding agents"
    cost_efficient: bool = True
    provider_priority: str = "quota_aware"
    switchyard: SwitchyardConfig = field(default_factory=SwitchyardConfig)


ROUTING_PRESETS: dict[str, RoutingProfile] = {
    "coding-agents": RoutingProfile(
        slug="coding-agents",
        label="Coding agents",
        cost_efficient=True,
        provider_priority="favor_cost",
    ),
    "coding-agents-experimental": RoutingProfile(
        slug="coding-agents-experimental",
        label="Coding agents (Switchyard)",
        cost_efficient=True,
        provider_priority="quota_aware",
        switchyard=SwitchyardConfig(
            enabled=True,
            efficient_model="antigravity/gemini-3.8-flash",
            capable_model="antigravity/gemini-4-neon",
            sensitivity="balanced",
        ),
    ),
    "production-agents": RoutingProfile(
        slug="production-agents",
        label="General purpose agents",
        cost_efficient=True,
        provider_priority="favor_cost",
    ),
    "low-latency": RoutingProfile(
        slug="low-latency",
        label="Low latency",
        cost_efficient=False,
        provider_priority="throughput_only",
    ),
    "automations": RoutingProfile(
        slug="automations",
        label="Automations",
        cost_efficient=True,
        provider_priority="cost_only",
    ),
    "knowledge-work": RoutingProfile(
        slug="knowledge-work",
        label="Research & analysis",
        cost_efficient=True,
        provider_priority="favor_cost",
    ),
}


def _catalog_entry(model: str) -> ModelCandidate | None:
    wanted = model.strip()
    for row in CATALOG:
        if row.model == wanted:
            return row
    return None


def validate_switchyard_pair(efficient_model: str, capable_model: str) -> str | None:
    """Validate a Ramp-style Switchyard pair (distinct + >= 2x output cost gate)."""
    low = efficient_model.strip()
    high = capable_model.strip()
    if not low or not high:
        return "Choose both an efficient and a capable model for Switchyard."
    if low == high:
        return "Choose different efficient and capable models."
    low_row = _catalog_entry(low)
    high_row = _catalog_entry(high)
    if low_row is not None and high_row is not None:
        if high_row.cost < 2.0 * low_row.cost:
            return "The capable model must cost at least 2x the efficient model on output tokens."
    return None


def load_routing_profile(path: Path | None = None) -> RoutingProfile:
    """Load active Ramp V2 routing profile from PA_ROUTER_PROFILE or router-profile.json."""
    env_val = (os.environ.get("PA_ROUTER_PROFILE") or "").strip()
    if env_val and env_val.lower() in ROUTING_PRESETS:
        return ROUTING_PRESETS[env_val.lower()]
    file_path = path
    if file_path is None:
        if env_val and Path(env_val).is_file():
            file_path = Path(env_val)
        else:
            file_path = harness_dir() / PROFILE_NAME
    try:
        raw = json.loads(file_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return RoutingProfile()
    if not isinstance(raw, dict):
        return RoutingProfile()
    slug = str(raw.get("preset_type") or raw.get("slug") or "coding-agents").strip().lower()
    base = ROUTING_PRESETS.get(slug, RoutingProfile())
    priority = str(
        raw.get("provider_routing_preference")
        or raw.get("provider_priority")
        or base.provider_priority
    ).strip().lower()
    if priority not in PROVIDER_PRIORITIES:
        priority = base.provider_priority
    cost_efficient = bool(
        raw.get("cost_efficient_routing_enabled", raw.get("cost_efficient", base.cost_efficient))
    )
    sw_raw = raw.get("switchyard_settings") if isinstance(raw.get("switchyard_settings"), dict) else {}
    sw_enabled = bool(raw.get("switchyard_routing_enabled", sw_raw.get("enabled", base.switchyard.enabled)))
    eff = str(sw_raw.get("efficient_model") or base.switchyard.efficient_model or "").strip()
    cap = str(sw_raw.get("capable_model") or base.switchyard.capable_model or "").strip()
    sens = str(
        sw_raw.get("sensitivity_config_name") or sw_raw.get("sensitivity") or base.switchyard.sensitivity
    ).strip().lower()
    if sens not in SWITCHYARD_SENSITIVITIES:
        sens = "balanced"
    switchyard = SwitchyardConfig(
        enabled=sw_enabled and bool(eff and cap) and validate_switchyard_pair(eff, cap) is None,
        efficient_model=eff,
        capable_model=cap,
        sensitivity=sens,
    )
    return RoutingProfile(
        slug=slug,
        label=str(raw.get("name") or base.label),
        cost_efficient=cost_efficient,
        provider_priority=priority,
        switchyard=switchyard,
    )


def detect_ramp_router_gateway(
    *,
    env: Mapping[str, str] | None = None,
    home: Path | None = None,
) -> dict[str, Any] | None:
    """Detect whether @ramp/router is configured locally (env or setup receipts)."""
    source_env = os.environ if env is None else env
    base_url = (source_env.get("ROUTER_BASE_URL") or "").strip()
    has_key = bool((source_env.get("ROUTER_API_KEY") or "").strip())
    if base_url and has_key:
        return {"source": "env", "base_url": base_url}
    user_home = Path.home() if home is None else home
    receipts = (
        ("claude-code", user_home / ".claude" / "ramp-router-state.json"),
        ("antigravity", user_home / ".gemini" / "config" / "ramp-router-state.json"),
    )
    configured: list[str] = []
    for harness, receipt in receipts:
        if receipt.is_file():
            configured.append(harness)
    if configured:
        return {"source": "receipt", "harnesses": tuple(configured)}
    return None


@dataclass(frozen=True)
class QuotaWindow:
    """One rate window inside a provider pool."""

    label: str
    used_percent: float
    resets_at: float | None = None
    reset_description: str | None = None
    window_minutes: int | None = None


@dataclass
class PoolQuota:
    """Every window that draws on one independent quota pool."""

    pool: str
    provider: str
    windows: list[QuotaWindow] = field(default_factory=list)
    error: str | None = None

    @property
    def used_percent(self) -> float:
        if self.error and not self.windows:
            return 100.0
        if not self.windows:
            return 0.0
        return max(window.used_percent for window in self.windows)

    def binding_window(self) -> QuotaWindow | None:
        if not self.windows:
            return None
        return max(self.windows, key=lambda window: (window.used_percent, window.label))


@dataclass
class QuotaSnapshot:
    """A parsed ``codexbar usage`` payload, possibly served from cache."""

    pools: dict[str, PoolQuota] = field(default_factory=dict)
    fetched_at: float = 0.0
    stale: bool = False
    error: str | None = None
    providers: tuple[str, ...] = ()

    @classmethod
    def empty(cls, *, error: str | None = None, now: float | None = None) -> QuotaSnapshot:
        return cls(fetched_at=now if now is not None else time.time(), error=error)


@dataclass
class ProviderHealth:
    """Online circuit-breaker and EWMA score for one quota pool."""

    pool: str
    failure_ewma: float = 0.0
    latency_ewma_ms: float = 0.0
    samples: int = 0
    latency_samples: int = 0
    open_until: float = 0.0
    last_error: str | None = None
    last_kind: str | None = None

    def is_open(self, now: float) -> bool:
        return now < self.open_until


@dataclass(frozen=True)
class TaskProfile:
    """The tier a task needs, after role signals and retry escalation."""

    tier: str
    thinking: str
    reasons: tuple[str, ...]
    escalated_from: str | None = None


@dataclass(frozen=True)
class RouteDecision:
    """The model a spawn should use, and why."""

    model: str
    thinking: str
    reason: str
    tier: str
    rerouted: bool
    fallbacks_tried: tuple[str, ...] = ()
    exhausted: bool = False
    reset_hint: str = ""

    def __post_init__(self) -> None:
        if isinstance(self.thinking, str) and self.thinking.strip().lower() == "xhigh":
            object.__setattr__(self, "thinking", "high")


def router_mode() -> str:
    """``off``, ``on``, or ``auto`` from ``PA_MODEL_ROUTER`` or ``router-profile.json``."""

    raw = (os.environ.get("PA_MODEL_ROUTER") or "").strip().lower()
    if raw in _OFF:
        return "off"
    if raw in _ON:
        return "on"
    if raw == "auto":
        return "auto"
    try:
        doc = json.loads((harness_dir() / PROFILE_NAME).read_text(encoding="utf-8"))
        if isinstance(doc, dict):
            saved = str(doc.get("routing_mode") or "").strip().lower()
            if saved in _OFF:
                return "off"
            if saved in _ON:
                return "on"
            if saved == "auto":
                return "auto"
    except (OSError, ValueError):
        pass
    return "auto"


def router_disabled() -> bool:
    return router_mode() == "off"


def router_explicit() -> bool:
    return router_mode() == "on"


def is_recognized_model(model: str | None) -> bool:
    """True for provider models this router knows how to move between.

    Unregistered test models (``pi/test-model``, ``m/x``, ``openai/gpt-5``)
    stay untouched so factory unit tests keep their spawn expectations.
    """

    if not isinstance(model, str):
        return False
    lowered = model.strip().lower()
    return any(lowered.startswith(prefix) for prefix in _RECOGNIZED_PREFIXES)


def harness_dir() -> Path:
    override = (os.environ.get("PA_ROUTER_HOME") or "").strip()
    if override:
        return Path(override).expanduser()
    return Path.home() / ".prime" / "harness"


def _agent_dir() -> Path:
    raw = (
        os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
        or os.environ.get("PI_CODING_AGENT_DIR")
        or str(Path.home() / ".prime" / "agent")
    )
    return Path(raw).expanduser()


def _read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _atomic_write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    # A per-writer temporary name keeps concurrent writers from replacing a half-written file.
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{threading.get_ident()}.tmp")
    temporary.write_text(text, encoding="utf-8")
    os.replace(temporary, path)


def _parse_time(value: Any) -> float | None:
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, (int, float)):
        number = float(value)
        if number > 10_000_000_000:
            return number / 1000.0
        if number > 1_000_000_000:
            return number
        return None
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.timestamp()


def _used_percent(value: Any) -> float | None:
    if not isinstance(value, dict):
        return None
    for key in ("usedPercent", "used_percent", "used"):
        raw = value.get(key)
        if isinstance(raw, (int, float)) and not isinstance(raw, bool):
            return max(0.0, min(100.0, float(raw)))
    remaining = value.get("remainingPercent", value.get("remaining_percent"))
    if isinstance(remaining, (int, float)) and not isinstance(remaining, bool):
        return max(0.0, min(100.0, 100.0 - float(remaining)))
    return None


def _window_from(label: str, value: Any) -> QuotaWindow | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return QuotaWindow(label=label or "limit", used_percent=max(0.0, min(100.0, float(value))))
    if isinstance(value, dict) and isinstance(value.get("window"), dict):
        # codexbar's extraRateWindows item: {"id", "title", "window": {usedPercent, ...}}.
        if not label:
            named = value.get("label") or value.get("title") or value.get("name") or value.get("id")
            label = named.strip() if isinstance(named, str) else ""
        value = value["window"]
    used = _used_percent(value)
    if used is None or not isinstance(value, dict):
        return None
    minutes = value.get("windowMinutes", value.get("window_minutes"))
    window_minutes = int(minutes) if isinstance(minutes, (int, float)) and not isinstance(minutes, bool) else None
    description = value.get("resetDescription", value.get("reset_description"))
    if not isinstance(description, str) or not description.strip():
        description = None
    else:
        description = " ".join(description.split())
    display = label
    named = value.get("label") or value.get("title") or value.get("name") or value.get("id")
    if isinstance(named, str) and named.strip() and not label:
        display = named.strip()
    return QuotaWindow(
        label=display or "limit",
        used_percent=used,
        resets_at=_parse_time(value.get("resetsAt", value.get("resets_at"))),
        reset_description=description,
        window_minutes=window_minutes,
    )


def _canonical_provider(name: str) -> str | None:
    key = name.strip().lower()
    aliases = {
        "claude": "claude",
        "anthropic": "claude",
        "claude-cli": "claude",
        "codex": "codex",
        "openai-codex": "codex",
        "antigravity": "antigravity",
        "agy": "antigravity",
        "cursor": "cursor",
        "cursor-cli": "cursor",
    }
    return aliases.get(key)


def _label_targets(provider: str, label: str) -> tuple[str, ...] | None:
    """Pools a window belongs to. ``None`` means every pool of that provider."""

    text = " ".join(label.lower().replace("_", " ").replace("/", " ").split())
    if provider == "antigravity":
        if "gemini" in text:
            return ("antigravity-gemini",)
        if any(token in text for token in ("claude", "gpt", "opus", "sonnet")):
            return ("antigravity-claude-gpt",)
        return None
    if provider == "cursor":
        if "third" in text:
            return ("cursor-third-party",)
        if "total" in text:
            return ("cursor-total",)
        if "cursor" in text or "composer" in text:
            return ("cursor",)
        return ("cursor-total",)
    if provider in _PROVIDER_POOLS:
        return _PROVIDER_POOLS[provider]
    return None


def _add_window(pools: dict[str, PoolQuota], pool: str, provider: str, window: QuotaWindow) -> None:
    row = pools.get(pool)
    if row is None:
        row = PoolQuota(pool=pool, provider=provider)
        pools[pool] = row
    row.windows.append(window)


def _iter_extra(extra: Any) -> list[tuple[str, Any]]:
    if isinstance(extra, list):
        rows: list[tuple[str, Any]] = []
        for item in extra:
            if isinstance(item, dict):
                label = item.get("label") or item.get("title") or item.get("name") or item.get("id") or ""
                rows.append((str(label), item))
        return rows
    if isinstance(extra, dict):
        rows = []
        for key, value in extra.items():
            label = str(key)
            if isinstance(value, dict):
                nested = value.get("label") or value.get("title") or value.get("name")
                if isinstance(nested, str) and nested.strip():
                    label = nested
            rows.append((label, value))
        return rows
    return []


def _provider_error_text(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, str):
        text = " ".join(value.split())
        return text or None
    if isinstance(value, dict):
        message = value.get("message")
        if isinstance(message, str) and message.strip():
            return " ".join(message.split())
        return None
    return None


def parse_quota_snapshot(payload: str | Any, *, now: float | None = None) -> QuotaSnapshot:
    """Parse ``codexbar usage --json-only`` into independent quota pools.

    Antigravity's ``extraRateWindows`` split Gemini from Claude/GPT. codexbar
    nests them at ``usage.extraRateWindows`` as ``{title, window: {...}}``;
    the older top-level ``extraRateWindows`` shape is also read. Claude
    keeps Session and Weekly, Codex keeps Weekly, and Cursor keeps Total,
    Cursor, and Third Party. A null window is skipped. Unknown providers are
    ignored. Malformed JSON yields an empty snapshot with ``error`` set.
    """

    moment = time.time() if now is None else now
    if isinstance(payload, str):
        try:
            parsed: Any = json.loads(payload)
        except ValueError as exc:
            return QuotaSnapshot.empty(error=f"unreadable usage output: {exc}", now=moment)
    else:
        parsed = payload
    entries: list[Any]
    if isinstance(parsed, dict):
        entries = [parsed]
    elif isinstance(parsed, list):
        entries = parsed
    else:
        return QuotaSnapshot.empty(error="unexpected usage output", now=moment)

    pools: dict[str, PoolQuota] = {}
    names: list[str] = []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        raw_name = entry.get("provider")
        if not isinstance(raw_name, str) or not raw_name.strip():
            continue
        names.append(raw_name.strip())
        provider = _canonical_provider(raw_name)
        if provider is None:
            continue
        usage = entry.get("usage") if isinstance(entry.get("usage"), dict) else {}
        labels = entry.get("rateWindowLabels") if isinstance(entry.get("rateWindowLabels"), dict) else {}
        error = _provider_error_text(entry.get("error"))
        specific: list[tuple[str, QuotaWindow]] = []
        shared: list[QuotaWindow] = []
        extras = _iter_extra(entry.get("extraRateWindows", entry.get("extra_rate_windows")))
        extras += _iter_extra(usage.get("extraRateWindows", usage.get("extra_rate_windows")))
        for label, value in extras:
            window = _window_from(label, value)
            if window is None:
                continue
            targets = _label_targets(provider, window.label)
            if targets is None:
                shared.append(window)
            else:
                specific.append((window.label, window))
                for pool in targets:
                    _add_window(pools, pool, provider, window)
        for slot in ("primary", "secondary", "tertiary"):
            raw_window = usage.get(slot) if isinstance(usage, dict) else None
            if raw_window is None:
                continue
            label = labels.get(slot) if isinstance(labels.get(slot), str) else ""
            window = _window_from(str(label), raw_window)
            if window is None:
                continue
            if not window.label or window.label == "limit":
                window = QuotaWindow(
                    label=str(label or slot),
                    used_percent=window.used_percent,
                    resets_at=window.resets_at,
                    reset_description=window.reset_description,
                    window_minutes=window.window_minutes,
                )
            targets = _label_targets(provider, window.label)
            if targets is None:
                shared.append(window)
            else:
                for pool in targets:
                    _add_window(pools, pool, provider, window)
        provider_pools = _PROVIDER_POOLS[provider]
        filled = [pool for pool in provider_pools if pool in pools and pools[pool].windows]
        shared_targets = [pool for pool in provider_pools if pool not in filled] if filled else list(provider_pools)
        for window in shared:
            for pool in shared_targets:
                _add_window(pools, pool, provider, window)
        if error:
            for pool in provider_pools:
                row = pools.get(pool)
                if row is None:
                    row = PoolQuota(pool=pool, provider=provider, error=error)
                    pools[pool] = row
                elif row.error is None:
                    row.error = error
    return QuotaSnapshot(pools=pools, fetched_at=moment, providers=tuple(names))


def _threshold(tier: str) -> float:
    if tier in ("fast", "balanced"):
        return SOFT_THRESHOLD
    if tier in ("strong", "frontier"):
        return HARD_THRESHOLD
    raise ValueError(f"unknown tier {tier!r}")


def _bump(tier: str, steps: int) -> str:
    rank = TIER_RANK.get(tier, TIER_RANK["balanced"])
    return TIERS[max(0, min(len(TIERS) - 1, rank + steps))]


def _phrase_hits(text: str, phrases: tuple[str, ...], words: tuple[str, ...]) -> list[str]:
    found: list[str] = []
    for phrase in phrases:
        if phrase in text:
            found.append(phrase)
    for word in words:
        if re.search(rf"\b{re.escape(word)}\b", text):
            found.append(word)
    return found


def classify_task(
    *,
    prompt: str = "",
    state_id: str = "",
    role: str | None = None,
    specialist: str | None = None,
    attempt: int = 1,
) -> TaskProfile:
    """Map factory signals onto a tier and a thinking effort.

    Attempt 1 uses the task. Each later attempt steps up one tier so a miss
    on a fast model retries on a stronger one, capping at frontier.
    """

    blob = " ".join(
        part.lower()
        for part in (state_id, role or "", specialist or "", prompt)
        if isinstance(part, str) and part.strip()
    )
    best = "balanced"
    reasons: list[str] = []
    frontier_hits = _phrase_hits(blob, _FRONTIER_PHRASES, _FRONTIER_WORDS)
    strong_hits = _phrase_hits(blob, _STRONG_PHRASES, _STRONG_WORDS)
    fast_hits = _phrase_hits(blob, _FAST_PHRASES, _FAST_WORDS)
    if fast_hits:
        best = "fast"
        reasons.append("fast signals: " + ", ".join(fast_hits[:4]))
    if strong_hits and TIER_RANK["strong"] > TIER_RANK[best]:
        best = "strong"
        reasons.append("strong signals: " + ", ".join(strong_hits[:4]))
    if frontier_hits and TIER_RANK["frontier"] > TIER_RANK[best]:
        best = "frontier"
        reasons.append("frontier signals: " + ", ".join(frontier_hits[:4]))
    for label, tokens in _STATE_TIERS:
        if any(token in state_id.lower() for token in tokens) and TIER_RANK[label] > TIER_RANK[best]:
            best = label
            reasons.append(f"state {state_id} -> {label}")
    for raw in (role, specialist):
        if not isinstance(raw, str):
            continue
        key = raw.strip().lower().replace("_", "-")
        leaf = key.split("/")[-1].split(":")[-1]
        mapped = _ROLE_TIERS.get(leaf) or _ROLE_TIERS.get(key)
        if mapped is None:
            for name, tier in _ROLE_TIERS.items():
                if name in leaf:
                    mapped = tier
                    break
        if mapped is not None and TIER_RANK[mapped] > TIER_RANK[best]:
            best = mapped
            reasons.append(f"role {raw.strip()} -> {mapped}")
    if not reasons:
        reasons.append("no specialty signal; balanced")
    steps = max(0, int(attempt) - 1)
    tier = _bump(best, steps)
    escalated = best if tier != best else None
    if escalated is not None:
        reasons.append(f"escalated {best} -> {tier} on attempt {attempt}")
    return TaskProfile(tier=tier, thinking=TIER_THINKING[tier], reasons=tuple(reasons), escalated_from=escalated)


def pool_for_model(model: str) -> str:
    for candidate in CATALOG:
        if candidate.model == model:
            return candidate.pool
    provider, _, model_id = model.partition("/")
    provider = provider.lower()
    model_id = model_id.lower()
    if provider in {"antigravity", "antigravity-cli"}:
        if any(token in model_id for token in ("claude", "gpt", "opus", "sonnet", "codex")):
            return "antigravity-claude-gpt"
        return "antigravity-gemini"
    if provider in {"cursor", "cursor-cli"}:
        if any(token in model_id for token in ("claude", "gpt", "gemini", "opus", "sonnet", "codex")):
            return "cursor-third-party"
        return "cursor"
    if provider in {"anthropic", "claude-cli"}:
        return "claude"
    if provider in {"openai-codex", "codex"}:
        return "codex"
    return provider or "unknown"


def provider_for_model(model: str) -> str:
    for candidate in CATALOG:
        if candidate.model == model:
            return candidate.provider
    pool = pool_for_model(model)
    if pool.startswith("antigravity"):
        return "antigravity"
    if pool.startswith("cursor"):
        return "cursor"
    if pool == "claude":
        return "claude"
    if pool == "codex":
        return "codex"
    return pool


def _pool_used(snapshot: QuotaSnapshot, pool: str) -> float:
    row = snapshot.pools.get(pool)
    if row is None:
        return 0.0
    return row.used_percent


def pool_pressure(snapshot: QuotaSnapshot, pool: str) -> float:
    """Used percent that actually blocks a pool, including Cursor's Total cap."""

    used = _pool_used(snapshot, pool)
    if pool in ("cursor", "cursor-third-party"):
        used = max(used, _pool_used(snapshot, "cursor-total"))
    return used


def _health_open(health: Mapping[str, ProviderHealth], pool: str, now: float) -> bool:
    row = health.get(pool)
    return row is not None and row.is_open(now)


def _failure_score(health: Mapping[str, ProviderHealth], pool: str) -> tuple[float, float]:
    row = health.get(pool)
    if row is None:
        return (0.0, 0.0)
    return (row.failure_ewma, row.latency_ewma_ms)


def _format_eta(resets_at: float | None, now: float) -> str:
    if resets_at is None:
        return "reset unknown"
    remaining = resets_at - now
    if remaining <= 0:
        return "reset due"
    minutes = int(remaining // 60)
    if minutes >= 48 * 60:
        return f"resets in {minutes // (60 * 24)}d"
    if minutes >= 60:
        return f"resets in {minutes // 60}h{minutes % 60:02d}m"
    return f"resets in {minutes}m"


_RESET_ANCHOR_RE = re.compile(r"reset|try again", re.IGNORECASE)
_DURATION_TOKEN_RE = re.compile(
    r"(\d+(?:\.\d+)?)\s*(days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)(?![a-z])",
    re.IGNORECASE,
)
_DURATION_GAP_RE = re.compile(r"[\s,]*(?:and\s+)?", re.IGNORECASE)
_DURATION_SECONDS = {"d": 86400, "h": 3600, "m": 60, "s": 1}
# How far past "resets"/"try again" the first duration may start ("resets in about 45m").
_RESET_LOOKAHEAD = 24


def _relative_reset(detail: str, now: float) -> float | None:
    """``now`` plus a duration such as ``4h52m9s``, ``59m9s``, ``45m``, or
    ``4 days, 9 hours`` that follows "reset" or "try again".

    A clock time ("resets 2am") or a date is not a duration and gives None.
    """

    for anchor in _RESET_ANCHOR_RE.finditer(detail):
        window = detail[anchor.end() : anchor.end() + _RESET_LOOKAHEAD]
        first = _DURATION_TOKEN_RE.search(window)
        if first is None:
            continue
        position = anchor.end() + first.start()
        total = 0.0
        while True:
            token = _DURATION_TOKEN_RE.match(detail, position)
            if token is None:
                break
            total += float(token.group(1)) * _DURATION_SECONDS[token.group(2)[0].lower()]
            position = _DURATION_GAP_RE.match(detail, token.end()).end()
        if total > 0:
            return now + total
    return None


PROVIDER_ERROR_MARKERS: tuple[str, ...] = (
    "invalid api key",
    "fix external api key",
    "authentication_error",
    "no api key found",
    "not logged in",
    "individual quota reached",
    "quota exceeded",
    "exceeded your current quota",
    "rate limit reached",
    "rate_limit_error",
    "too many requests",
    "please upgrade your subscription",
    "you've hit your limit",
    "you have hit your limit",
    "hit your limit",
    "usage limit",
    "out of extra usage",
    "credit balance is too low",
    "overloaded_error",
)

_HTTP_429_RE = re.compile(r"(?:http|status|error|code)[^a-z0-9]{0,4}429\b|\b429\s+(?:too many|rate)", re.IGNORECASE)


def is_provider_error_text(text: str | None) -> bool:
    """True when ``text`` describes a provider quota, rate-limit, or auth failure."""

    if not isinstance(text, str) or not text.strip():
        return False
    lowered = text.lower()
    if any(marker in lowered for marker in PROVIDER_ERROR_MARKERS):
        return True
    return _HTTP_429_RE.search(text) is not None


# Narrow subset of unmistakable provider CLI auth/quota banners that a CLI
# subprocess may print as plain stdout with exit code 0 / status "done".
# Generic engineering phrases ("429", "too many requests", "usage limit",
# "rate limit reached", "quota exceeded") are intentionally excluded so short
# coding answers that discuss rate limiting never false-positive.
CLI_BANNER_ERROR_MARKERS: tuple[str, ...] = (
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


def is_cli_banner_error_text(text: str | None) -> bool:
    """True only when ``text`` matches an unmistakable provider auth/quota banner."""

    if not isinstance(text, str) or not text.strip():
        return False
    lowered = text.lower()
    return any(marker in lowered for marker in CLI_BANNER_ERROR_MARKERS)


_SESSION_ERROR_DETAIL_CAP = 220


def _cap_error_detail(text: str, cap: int = _SESSION_ERROR_DETAIL_CAP) -> str:
    detail = " ".join(text.split())
    if len(detail) > cap:
        detail = detail[: cap - 1].rstrip() + "…"
    return detail


def scan_session_jsonl_quota_error(
    path: Path | str | None,
    offset: int = 0,
    *,
    tail_bytes: int = 32_768,
    tail_lines: int = 40,
) -> str | None:
    """Scan newly written tail rows of a session JSONL for a provider quota/auth error.

    Matches the durable row shapes emitted by ``radiata-daemon``:
    - ``type == "custom_message"`` with ``customType == "provider_retry_outcome"`` and
      ``details.success is False`` (never matches recovered episodes).
    - ``type == "message"`` with a provider-error ``message.errorMessage``.
    """

    if not path:
        return None
    file_path = Path(path)
    try:
        size = file_path.stat().st_size
    except OSError:
        return None
    start_offset = max(0, int(offset or 0))
    if size <= start_offset:
        return None
    seek_pos = max(start_offset, size - tail_bytes)
    try:
        with file_path.open("rb") as fh:
            fh.seek(seek_pos)
            chunk = fh.read().decode("utf-8", errors="replace")
    except OSError:
        return None
    lines = [line for line in chunk.splitlines() if line.strip()][-tail_lines:]
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
            if isinstance(msg, str) and is_provider_error_text(msg):
                return _cap_error_detail(msg)
        if row.get("type") == "message":
            msg = row.get("message")
            if not isinstance(msg, dict):
                continue
            err_msg = msg.get("errorMessage")
            if isinstance(err_msg, str) and is_provider_error_text(err_msg):
                return _cap_error_detail(err_msg)
    return None


def failure_kind(detail: str) -> str:
    text = detail.lower()
    if any(marker in text for marker in ("invalid api key", "authentication", "not logged in", "no api key", "unauthorized")):
        return "auth"
    if any(marker in text for marker in ("429", "rate limit", "rate_limit_error", "too many requests", "overloaded_error")):
        return "rate_limit"
    return "quota"


def _env_flag(name: str) -> str | None:
    raw = os.environ.get(name)
    if raw is None:
        return None
    return raw.strip()


def _executable(path: Path) -> bool:
    return path.is_file() and os.access(path, os.X_OK)


def _which(name: str) -> bool:
    return shutil.which(name) is not None


def claude_cli_available() -> bool:
    """Mirror ``radiata_ai::claude_cli::is_available`` / factory's CLI probe."""

    explicit = _env_flag("PA_CLAUDE_BIN")
    if explicit is not None:
        if explicit.lower() == "none" or explicit == "0":
            return False
        if explicit:
            return True
    if _which("claude"):
        return True
    home = os.environ.get("HOME")
    if home:
        root = Path(home)
        for relative in (".local/bin/claude", ".claude/local/claude"):
            if _executable(root / relative):
                return True
    return False


def _real_anthropic_key(key: Any) -> bool:
    return isinstance(key, str) and bool(key.strip()) and "sk-ant-oat" not in key


def anthropic_api_configured() -> bool:
    """True when Anthropic is explicitly on the Messages API, matching the registry."""

    opt_in = (_env_flag("PA_ANTHROPIC_USE_API_KEY") or "").lower()
    if opt_in in {"1", "true"} and _real_anthropic_key(os.environ.get("ANTHROPIC_API_KEY")):
        return True
    agent_dir = _agent_dir()
    auth = _read_json(agent_dir / "auth.json")
    if isinstance(auth, dict):
        cred = auth.get("anthropic")
        if isinstance(cred, dict) and cred.get("type") == "api_key" and _real_anthropic_key(cred.get("key")):
            return True
    models_doc = _read_json(agent_dir / "models.json")
    if isinstance(models_doc, dict):
        providers = models_doc.get("providers")
        if isinstance(providers, dict):
            cfg = providers.get("anthropic")
            if isinstance(cfg, dict) and (
                cfg.get("headers")
                or cfg.get("authHeader") is not None
                or _real_anthropic_key(cfg.get("apiKey"))
            ):
                return True
    return False


def anthropic_uses_claude_cli() -> bool:
    """Mirror ``ModelRegistry::anthropic_uses_claude_cli``: ``anthropic/*`` routes through
    the ``claude`` CLI whenever it is installed and no explicit Messages API override is set."""

    return claude_cli_available() and not anthropic_api_configured()


def antigravity_available() -> bool:
    explicit = _env_flag("PA_ANTIGRAVITY_BIN")
    if explicit is not None:
        if explicit.lower() == "none" or explicit == "0":
            return False
        if explicit:
            return True
    if _which("agy") or _which("antigravity"):
        return True
    home = os.environ.get("HOME")
    return bool(home and _executable(Path(home) / ".local/bin/agy"))


def cursor_available() -> bool:
    """Mirror the Cursor CLI lookup: ``cursor-agent``, not a random ``agent`` on PATH."""

    explicit = _env_flag("PA_CURSOR_BIN")
    if explicit is not None:
        if explicit.lower() == "none" or explicit == "0":
            return False
        if explicit:
            return True
    if _which("cursor-agent"):
        return True
    home = os.environ.get("HOME")
    if not home:
        return False
    root = Path(home)
    return _executable(root / ".local/bin/cursor-agent") or _executable(root / ".local/bin/agent")


def codex_available() -> bool:
    explicit = _env_flag("PA_CODEX_BIN")
    if explicit is not None:
        if explicit.lower() == "none" or explicit == "0":
            return False
        if explicit:
            return True
    if _which("codex"):
        return True
    auth = _read_json(_agent_dir() / "auth.json")
    if isinstance(auth, dict):
        for key in ("openai-codex", "codex"):
            cred = auth.get(key)
            if isinstance(cred, dict) and (cred.get("type") or cred.get("access") or cred.get("key")):
                return True
    return False


def available_providers() -> set[str]:
    """Providers this machine can actually launch.

    ``PA_ROUTER_PROVIDERS`` replaces detection (tests and explicit config).
    An empty value means none.
    """

    override = os.environ.get("PA_ROUTER_PROVIDERS")
    if override is not None:
        return {part.strip().lower() for part in override.split(",") if part.strip()}
    found: set[str] = set()
    if claude_cli_available() or anthropic_api_configured():
        found.add("claude")
    if antigravity_available():
        found.add("antigravity")
    if cursor_available():
        found.add("cursor")
    if codex_available():
        found.add("codex")
    return found


def _catalog_tier(model: str) -> str | None:
    """The highest tier ``model`` serves in the catalog, or None for an unlisted model."""

    tiers = [row.tier for row in CATALOG if row.model == model]
    return max(tiers, key=TIER_RANK.__getitem__) if tiers else None


def _pin_pressure_ok(snapshot: QuotaSnapshot, pool: str, tier: str) -> bool:
    return pool_pressure(snapshot, pool) < _threshold(tier)


def _reset_hint(snapshot: QuotaSnapshot, pools: list[str], now: float) -> str:
    parts: list[str] = []
    seen: set[str] = set()
    for pool in pools:
        if pool in seen:
            continue
        seen.add(pool)
        row = snapshot.pools.get(pool)
        if row is None or not row.windows:
            parts.append(pool)
            continue
        window = row.binding_window()
        if window is None:
            continue
        parts.append(f"{pool} {window.used_percent:.0f}% ({_format_eta(window.resets_at, now)})")
    return ", ".join(parts)


def _switchyard_target(
    profile: TaskProfile,
    switchyard: SwitchyardConfig,
    *,
    attempt: int,
) -> str:
    """Pick efficient vs capable model under Ramp Switchyard sensitivity rules."""
    if not switchyard.enabled:
        return ""
    rank = TIER_RANK.get(profile.tier, 1)
    if attempt > 1:
        return switchyard.capable_model
    if switchyard.sensitivity == "frugal":
        # Only frontier goes directly to capable on attempt 1.
        return switchyard.capable_model if rank >= TIER_RANK["frontier"] else switchyard.efficient_model
    if switchyard.sensitivity == "quality":
        # Everything above fast uses capable.
        return switchyard.capable_model if rank >= TIER_RANK["balanced"] else switchyard.efficient_model
    # Balanced: fast/balanced -> efficient; strong/frontier -> capable.
    return switchyard.capable_model if rank >= TIER_RANK["strong"] else switchyard.efficient_model


def route_model(
    *,
    prompt: str = "",
    state_id: str = "",
    role: str | None = None,
    specialist: str | None = None,
    attempt: int = 1,
    pinned_model: str | None = None,
    pinned_thinking: str | None = None,
    snapshot: QuotaSnapshot | None = None,
    health: Mapping[str, ProviderHealth] | None = None,
    available: set[str] | None = None,
    now: float | None = None,
    exclude_models: tuple[str, ...] = (),
    routing_profile: RoutingProfile | None = None,
) -> RouteDecision:
    """Choose ``(model, thinking)`` for one admission.

    An explicit pin is kept when its pool is under the threshold for this
    task's tier and its circuit is closed. Soft threshold (90%) applies to
    fast and balanced work; hard threshold (98%) applies to strong and
    frontier work, including a pinned frontier model. A pin that must leave
    (excluded after a failure, hard-exhausted, circuit-open, or not installed)
    or that is retried keeps at least its catalog tier; only soft steering
    below the hard line may move it to a cheaper tier. When Ramp-style
    Switchyard is enabled on an unpinned spawn (or after a pin's pool is
    exhausted), tasks route cleanly between ``efficient_model`` and
    ``capable_model``. A closed set of eligible pools returns ``exhausted``
    instead of a model.
    """

    moment = time.time() if now is None else now
    active_profile = routing_profile if routing_profile is not None else RoutingProfile()
    profile = classify_task(
        prompt=prompt,
        state_id=state_id,
        role=role,
        specialist=specialist,
        attempt=max(1, int(attempt)),
    )
    quota = snapshot if snapshot is not None else QuotaSnapshot.empty(now=moment)
    scores = health if health is not None else {}
    providers = available if available is not None else available_providers()
    excluded = {model for model in exclude_models if isinstance(model, str) and model}
    pin = pinned_model.strip() if isinstance(pinned_model, str) and pinned_model.strip() else ""
    pin_pool = pool_for_model(pin) if pin else ""
    pin_provider = provider_for_model(pin) if pin else ""

    def viable(candidate: ModelCandidate, tier: str) -> bool:
        if candidate.model in excluded:
            return False
        if candidate.provider not in providers:
            return False
        if _health_open(scores, candidate.pool, moment):
            return False
        if candidate.pool.startswith("cursor") and _health_open(scores, "cursor-total", moment):
            return False
        return _pin_pressure_ok(quota, candidate.pool, tier)

    task_tier = classify_task(
        prompt=prompt, state_id=state_id, role=role, specialist=specialist, attempt=1
    ).tier
    pin_hard_blocked = bool(pin) and (
        pin in excluded
        or pin_provider not in providers
        or _health_open(scores, pin_pool, moment)
        or (pin_pool.startswith("cursor") and _health_open(scores, "cursor-total", moment))
        or pool_pressure(quota, pin_pool) >= HARD_THRESHOLD
    )
    if pin and attempt <= 1 and not pin_hard_blocked and _pin_pressure_ok(quota, pin_pool, task_tier):
        return RouteDecision(
            model=pin,
            thinking=pinned_thinking or "",
            reason=f"pinned {pin} kept; {pin_pool} under {_threshold(task_tier):.0f}% for {task_tier}",
            tier=task_tier,
            rerouted=False,
            fallbacks_tried=(),
        )

    # A pin that leaves for a hard reason (failure, exhausted or open pool, missing
    # provider) or on a retry keeps at least its own tier. Only soft steering (the
    # pin's pool between the soft and hard lines) may move work to a cheaper tier.
    soft_steer = bool(pin) and not pin_hard_blocked and not _pin_pressure_ok(quota, pin_pool, task_tier)
    pin_tier = _catalog_tier(pin) if pin else None
    if pin_tier is not None and not soft_steer and TIER_RANK[pin_tier] > TIER_RANK[profile.tier]:
        profile = TaskProfile(
            tier=pin_tier,
            thinking=TIER_THINKING[pin_tier],
            reasons=(*profile.reasons, f"kept pinned {pin} tier {pin_tier}"),
            escalated_from=profile.escalated_from,
        )

    # Ramp Switchyard 2-tier fast path (when enabled and pair target is healthy).
    sw_model = _switchyard_target(profile, active_profile.switchyard, attempt=max(1, int(attempt)))
    if sw_model and sw_model not in excluded:
        sw_provider = provider_for_model(sw_model)
        sw_pool = pool_for_model(sw_model)
        if (
            sw_provider in providers
            and not _health_open(scores, sw_pool, moment)
            and not (sw_pool.startswith("cursor") and _health_open(scores, "cursor-total", moment))
            and _pin_pressure_ok(quota, sw_pool, profile.tier)
        ):
            role_kind = "capable" if sw_model == active_profile.switchyard.capable_model else "efficient"
            return RouteDecision(
                model=sw_model,
                thinking=TIER_THINKING.get(profile.tier, "medium"),
                reason=(
                    f"switchyard ({active_profile.switchyard.sensitivity}) selected {role_kind} "
                    f"{sw_model} for {profile.tier}"
                ),
                tier=profile.tier,
                rerouted=bool(pin) and sw_model != pin,
                fallbacks_tried=(),
            )

    rejected: list[str] = []
    blocked_pools: list[str] = []
    chosen: ModelCandidate | None = None
    chosen_tier = profile.tier
    priority = active_profile.provider_priority
    for step in range(len(TIERS)):
        tier = _bump(profile.tier, step)
        rows = [row for row in CATALOG if row.tier == tier]
        viable_rows = []
        for row in rows:
            if row.provider not in providers:
                continue
            if viable(row, tier):
                viable_rows.append(row)
            else:
                if row.model not in rejected:
                    rejected.append(row.model)
                if row.pool not in blocked_pools:
                    blocked_pools.append(row.pool)
        if not viable_rows:
            continue
        if pin:
            for row in viable_rows:
                if row.model == pin:
                    chosen = row
                    chosen_tier = tier
                    break
        if chosen is None:
            def sort_key(row: ModelCandidate) -> tuple[Any, ...]:
                same_pool = 0 if pin_pool and row.pool == pin_pool else 1
                failure, latency = _failure_score(scores, row.pool)
                pressure = pool_pressure(quota, row.pool)
                if priority == "throughput_only":
                    return (latency, failure, pressure, row.cost, row.model)
                if priority == "favor_throughput":
                    return (same_pool, latency, failure, row.cost, pressure, row.model)
                if priority == "cost_only":
                    return (row.cost, failure, latency, row.model)
                if priority == "favor_cost":
                    return (same_pool, row.cost, failure, latency, row.model)
                # quota_aware (default): preserve the pin pool, then prefer more headroom in
                # coarse 25% pressure bands (so small differences do not thrash between
                # healthy pools), then low cost and reliability.
                return (same_pool, int(pressure // 25), row.cost, failure, latency, row.model)

            chosen = min(viable_rows, key=sort_key)
            chosen_tier = tier
        break

    hint = _reset_hint(quota, blocked_pools, moment)
    if chosen is None:
        reason = f"No healthy fallback for tier {profile.tier}."
        if hint:
            reason += f" Exhausted pools: {hint}."
        else:
            reason += " No installed provider can take this tier."
        return RouteDecision(
            model="",
            thinking="",
            reason=reason,
            tier=profile.tier,
            rerouted=bool(pin),
            fallbacks_tried=tuple(rejected),
            exhausted=True,
            reset_hint=hint,
        )
    rerouted = bool(pin) and chosen.model != pin
    if rerouted:
        why = f"rerouted {pin} -> {chosen.model} ({chosen_tier})"
        if hint:
            why += f"; avoided {hint}"
        else:
            why += f"; {profile.reasons[-1]}"
    elif not pin:
        why = f"selected {chosen.model} for {chosen_tier} ({profile.reasons[-1]})"
    else:
        why = f"pinned {chosen.model} still the best healthy {chosen_tier}"
    if chosen.model == pin:
        thinking = pinned_thinking or ""
    else:
        thinking = chosen.thinking
    return RouteDecision(
        model=chosen.model,
        thinking=thinking,
        reason=why,
        tier=chosen_tier,
        rerouted=rerouted,
        fallbacks_tried=tuple(rejected),
        reset_hint=hint,
    )


def find_codexbar() -> Path | None:
    explicit = _env_flag("PA_CODEXBAR_BIN")
    if explicit is not None:
        if explicit.lower() in {"", "none", "0"}:
            return None
        return Path(explicit)
    found = shutil.which("codexbar")
    if found:
        return Path(found)
    for candidate in ("/opt/homebrew/bin/codexbar", "/usr/local/bin/codexbar"):
        path = Path(candidate)
        if _executable(path):
            return path
    return None


_MEMORY_QUOTA: dict[str, tuple[float, str]] = {}
_MEMORY_QUOTA_ERRORS: dict[str, tuple[float, str]] = {}
_QUOTA_ERROR_TTL_SECONDS = 30.0
_BACKGROUND_REFRESH_GUARD_SECONDS = 30.0


def _spawn_background_quota_refresh(path: Path, timeout: float) -> None:
    """Refresh ``router-quota-cache.json`` in a detached subprocess so callers never stall."""

    binary = find_codexbar()
    if binary is None:
        return
    stamp = path.with_suffix(".refreshing")
    now = time.time()
    try:
        if stamp.exists() and now - stamp.stat().st_mtime < _BACKGROUND_REFRESH_GUARD_SECONDS:
            return
        stamp.parent.mkdir(parents=True, exist_ok=True)
        stamp.write_text(str(os.getpid()), encoding="utf-8")
    except OSError:
        return
    script = (
        "import json, os, pathlib, subprocess, sys, time\n"
        "binary, out_path, stamp_path, timeout = sys.argv[1], pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3]), float(sys.argv[4])\n"
        "try:\n"
        "    res = subprocess.run([binary, 'usage', '--json-only'], stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout, check=False)\n"
        "    if res.stdout.strip():\n"
        "        out_path.parent.mkdir(parents=True, exist_ok=True)\n"
        "        tmp = out_path.with_name(f'.{out_path.name}.{os.getpid()}.tmp')\n"
        "        tmp.write_text(json.dumps({'fetched_at': time.time(), 'raw': res.stdout}), encoding='utf-8')\n"
        "        os.replace(tmp, out_path)\n"
        "except Exception:\n"
        "    pass\n"
        "finally:\n"
        "    try:\n"
        "        stamp_path.unlink(missing_ok=True)\n"
        "    except OSError:\n"
        "        pass\n"
    )
    try:
        subprocess.Popen(
            [sys.executable, "-c", script, str(binary), str(path), str(stamp), str(timeout)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            close_fds=True,
        )
    except OSError:
        try:
            stamp.unlink(missing_ok=True)
        except OSError:
            pass


def load_quota_snapshot(
    *,
    now: float | None = None,
    runner: Callable[[float], str] | None = None,
    cache_path: Path | None = None,
    ttl: float = QUOTA_CACHE_TTL_SECONDS,
    timeout: float = QUOTA_FETCH_TIMEOUT_SECONDS,
    allow_blocking: bool = True,
) -> QuotaSnapshot:
    """Load quota, from memory, then disk, then ``codexbar``.

    A fresh cache returns without a subprocess. When a disk cache exists and
    is stale (and no custom test ``runner`` is supplied), stale-while-revalidate
    returns the cached snapshot immediately and refreshes ``codexbar`` in a
    detached background subprocess so prompts and spawns never block for
    seconds. ``allow_blocking=False`` also avoids blocking on a cold cache.
    ``PA_ROUTER_QUOTA_JSON`` short-circuits the CLI for tests and operators.
    """

    moment = time.time() if now is None else now
    inline = os.environ.get("PA_ROUTER_QUOTA_JSON")
    if inline is not None and runner is None:
        snapshot = parse_quota_snapshot(inline, now=moment)
        snapshot.fetched_at = moment
        return snapshot
    path = cache_path or (harness_dir() / CACHE_NAME)
    key = str(path)

    def from_raw(raw: str, fetched_at: float, *, stale: bool, error: str | None) -> QuotaSnapshot:
        snapshot = parse_quota_snapshot(raw, now=fetched_at)
        snapshot.fetched_at = fetched_at
        snapshot.stale = stale
        if error and snapshot.error is None:
            snapshot.error = error
        return snapshot

    cached = _MEMORY_QUOTA.get(key)
    if cached is not None and moment - cached[0] <= ttl:
        return from_raw(cached[1], cached[0], stale=False, error=None)
    disk_raw: str | None = None
    disk_at: float | None = None
    document = _read_json(path)
    if isinstance(document, dict) and isinstance(document.get("raw"), str):
        disk_raw = document["raw"]
        fetched = document.get("fetched_at")
        if isinstance(fetched, (int, float)) and not isinstance(fetched, bool):
            disk_at = float(fetched)
    if disk_raw is not None and disk_at is not None and moment - disk_at <= ttl:
        _MEMORY_QUOTA[key] = (disk_at, disk_raw)
        return from_raw(disk_raw, disk_at, stale=False, error=None)

    if runner is None and (disk_raw is not None and disk_at is not None):
        _spawn_background_quota_refresh(path, timeout)
        _MEMORY_QUOTA[key] = (disk_at, disk_raw)
        return from_raw(disk_raw, disk_at, stale=True, error=None)

    if runner is None and not allow_blocking:
        _spawn_background_quota_refresh(path, timeout)
        return QuotaSnapshot.empty(now=moment)

    if runner is None:
        failed = _MEMORY_QUOTA_ERRORS.get(key)
        if failed is not None and moment - failed[0] <= _QUOTA_ERROR_TTL_SECONDS:
            return QuotaSnapshot.empty(error=failed[1], now=moment)

    def remember(raw: str, fetched_at: float) -> None:
        _MEMORY_QUOTA[key] = (fetched_at, raw)
        _MEMORY_QUOTA_ERRORS.pop(key, None)
        try:
            _atomic_write(path, json.dumps({"fetched_at": fetched_at, "raw": raw}))
        except OSError:
            return

    def fail(message: str) -> QuotaSnapshot:
        if runner is None:
            _MEMORY_QUOTA_ERRORS[key] = (moment, message)
        if disk_raw is not None and disk_at is not None:
            return from_raw(disk_raw, disk_at, stale=True, error=message)
        if cached is not None:
            return from_raw(cached[1], cached[0], stale=True, error=message)
        return QuotaSnapshot.empty(error=message, now=moment)

    fetch = runner or _run_codexbar
    try:
        raw = fetch(timeout)
    except subprocess.TimeoutExpired:
        return fail(f"codexbar timed out after {timeout:g}s")
    except (OSError, RuntimeError, ValueError) as exc:
        return fail(str(exc) or "codexbar failed")
    if not isinstance(raw, str) or not raw.strip():
        return fail("codexbar returned empty output")
    remember(raw, moment)
    return from_raw(raw, moment, stale=False, error=None)


def _run_codexbar(timeout: float) -> str:
    binary = find_codexbar()
    if binary is None:
        raise FileNotFoundError("codexbar not found")
    completed = subprocess.run(
        [str(binary), "usage", "--json-only"],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    if completed.stdout.strip():
        return completed.stdout
    detail = (completed.stderr or "").strip() or f"codexbar exited {completed.returncode}"
    raise RuntimeError(detail)


def _ewma(previous: float, sample: float, samples: int) -> float:
    if samples <= 1:
        return sample
    return EWMA_ALPHA * sample + (1.0 - EWMA_ALPHA) * previous


class ModelRouter:
    """Pool health shared through ``router-health.json`` plus the pure routing functions.

    Each process keeps a copy of the file. A health update takes an exclusive
    lock on ``router-health.lock``, re-reads the file, changes only its own
    pool, and writes the file back, so concurrent factories and Captains never
    drop each other's pools.
    """

    def __init__(self, home: Path | None = None) -> None:
        self.home = home or harness_dir()
        self.health: dict[str, ProviderHealth] = {}
        self._health_mtime_ns: int | None = None
        self._load_health()

    def _health_path(self) -> Path:
        return self.home / HEALTH_NAME

    def _current_health_mtime_ns(self) -> int | None:
        try:
            return self._health_path().stat().st_mtime_ns
        except OSError:
            return None

    def reload_health_if_changed(self) -> None:
        mtime = self._current_health_mtime_ns()
        if mtime is not None and mtime != self._health_mtime_ns:
            self._load_health()

    def _load_health(self) -> None:
        path = self._health_path()
        try:
            self._health_mtime_ns = path.stat().st_mtime_ns
        except OSError:
            self._health_mtime_ns = None
            return
        document = _read_json(path)
        pools = document.get("pools") if isinstance(document, dict) else None
        if not isinstance(pools, dict):
            return
        loaded: dict[str, ProviderHealth] = {}
        for pool, row in pools.items():
            if not isinstance(pool, str) or not isinstance(row, dict):
                continue
            loaded[pool] = ProviderHealth(
                pool=pool,
                failure_ewma=float(row.get("failure_ewma") or 0.0),
                latency_ewma_ms=float(row.get("latency_ewma_ms") or 0.0),
                samples=int(row.get("samples") or 0),
                latency_samples=int(row.get("latency_samples") or 0),
                open_until=float(row.get("open_until") or 0.0),
                last_error=row.get("last_error") if isinstance(row.get("last_error"), str) else None,
                last_kind=row.get("last_kind") if isinstance(row.get("last_kind"), str) else None,
            )
        self.health = loaded

    @contextmanager
    def _health_update(self) -> Any:
        """Hold the cross-process health lock with ``self.health`` re-read from disk.

        Health is advisory: when the lock cannot be taken, the update stays in
        this process and ``save_health`` writes it without the merge.
        """

        lock_path = self.home / HEALTH_LOCK_NAME
        handle = None
        try:
            lock_path.parent.mkdir(parents=True, exist_ok=True)
            handle = lock_path.open("a+b")
            fcntl.flock(handle, fcntl.LOCK_EX)
        except OSError:
            if handle is not None:
                handle.close()
            yield
            return
        with handle:
            try:
                self._load_health()
                yield
            finally:
                fcntl.flock(handle, fcntl.LOCK_UN)

    def save_health(self) -> None:
        payload = {
            "pools": {
                pool: {
                    "failure_ewma": row.failure_ewma,
                    "latency_ewma_ms": row.latency_ewma_ms,
                    "samples": row.samples,
                    "latency_samples": row.latency_samples,
                    "open_until": row.open_until,
                    "last_error": row.last_error,
                    "last_kind": row.last_kind,
                }
                for pool, row in self.health.items()
            }
        }
        try:
            _atomic_write(self._health_path(), json.dumps(payload))
            self._health_mtime_ns = self._current_health_mtime_ns()
        except OSError:
            return

    def pool_open(self, pool: str, now: float | None = None) -> bool:
        return _health_open(self.health, pool, time.time() if now is None else now)

    def record_success(self, pool: str, latency_ms: float, *, now: float | None = None) -> None:
        moment = time.time() if now is None else now
        with self._health_update():
            row = self.health.get(pool) or ProviderHealth(pool=pool)
            row.samples += 1
            row.latency_samples += 1
            row.failure_ewma = _ewma(row.failure_ewma, 0.0, row.samples)
            row.latency_ewma_ms = _ewma(row.latency_ewma_ms, max(0.0, latency_ms), row.latency_samples)
            if row.open_until <= moment:
                row.last_kind = None
            self.health[pool] = row
            self.save_health()

    def record_failure(
        self,
        pool: str,
        detail: str,
        *,
        now: float | None = None,
        reset_at: float | None = None,
        kind: str | None = None,
    ) -> ProviderHealth:
        moment = time.time() if now is None else now
        failure = kind or failure_kind(detail)
        if failure == "auth":
            opens = moment + AUTH_COOLDOWN_SECONDS
        elif failure == "rate_limit":
            opens = moment + RATE_LIMIT_COOLDOWN_SECONDS
            if reset_at is not None and moment < reset_at < opens:
                opens = reset_at
        elif reset_at is not None and reset_at > moment:
            opens = reset_at
        else:
            opens = moment + QUOTA_COOLDOWN_SECONDS
        with self._health_update():
            row = self.health.get(pool) or ProviderHealth(pool=pool)
            row.samples += 1
            row.failure_ewma = _ewma(row.failure_ewma, 1.0, row.samples)
            row.last_error = " ".join(detail.split())[:220]
            row.last_kind = failure
            row.open_until = opens
            self.health[pool] = row
            self.save_health()
        return row

    def record_model_failure(self, model: str, detail: str, *, snapshot: QuotaSnapshot | None = None, now: float | None = None) -> None:
        """Open ``model``'s pool until the reset the error names.

        Without a reset in the error text, the pool stays closed until the
        latest reset among its windows at or above HARD_THRESHOLD. A window
        under that line did not cause the failure, so its reset (often a
        weekly one, days away) is never used.
        """

        moment = time.time() if now is None else now
        pool = pool_for_model(model)
        reset_at = _relative_reset(detail, moment)
        if reset_at is None and snapshot is not None:
            reset_at = _exhausted_reset(snapshot, pool, moment)
        self.record_failure(pool, detail, now=moment, reset_at=reset_at)

    def route(self, **kwargs: Any) -> RouteDecision:
        kwargs.setdefault("health", self.health)
        kwargs.setdefault("snapshot", load_quota_snapshot())
        kwargs.setdefault("available", available_providers())
        return route_model(**kwargs)


def _exhausted_reset(snapshot: QuotaSnapshot, pool: str, now: float) -> float | None:
    """The latest future reset among ``pool``'s windows at or above HARD_THRESHOLD."""

    pools = [pool, "cursor-total"] if pool in ("cursor", "cursor-third-party") else [pool]
    resets = [
        window.resets_at
        for name in pools
        if (row := snapshot.pools.get(name)) is not None
        for window in row.windows
        if window.used_percent >= HARD_THRESHOLD and window.resets_at is not None and window.resets_at > now
    ]
    return max(resets) if resets else None


_ROUTER: ModelRouter | None = None


def get_router() -> ModelRouter:
    global _ROUTER
    home = harness_dir()
    if _ROUTER is None or _ROUTER.home != home:
        _ROUTER = ModelRouter(home=home)
    else:
        _ROUTER.reload_health_if_changed()
    return _ROUTER


def reset_router() -> None:
    """Drop the process router. Tests call this so health does not leak."""

    global _ROUTER
    _ROUTER = None
    _MEMORY_QUOTA.clear()
    _MEMORY_QUOTA_ERRORS.clear()


def note_provider_failure(model: str | None, detail: str, *, now: float | None = None) -> None:
    if not isinstance(model, str) or not model.strip():
        return
    snapshot = load_quota_snapshot(now=now, allow_blocking=False)
    get_router().record_model_failure(model, detail, snapshot=snapshot, now=now)


def note_provider_success(model: str | None, latency_ms: float | None, *, now: float | None = None) -> None:
    """Record a clean turn on ``model``'s pool so its failure score decays."""

    if router_disabled() or not is_recognized_model(model):
        return
    assert model is not None
    get_router().record_success(pool_for_model(model.strip()), float(latency_ms or 0.0), now=now)


def pool_is_hard_exhausted(
    model: str | None,
    *,
    snapshot: QuotaSnapshot | None = None,
    health: Mapping[str, ProviderHealth] | None = None,
    now: float | None = None,
) -> str | None:
    """Return a short reason if ``model``'s pool is >= HARD_THRESHOLD or circuit-open."""

    if not is_recognized_model(model):
        return None
    assert model is not None
    moment = time.time() if now is None else now
    pool = pool_for_model(model)
    scores = health if health is not None else get_router().health
    if _health_open(scores, pool, moment):
        row = scores.get(pool)
        reason = (row.last_error or row.last_kind or "cooldown") if row is not None else "cooldown"
        return f"{pool} circuit is open ({reason})"
    if pool.startswith("cursor") and _health_open(scores, "cursor-total", moment):
        row = scores.get("cursor-total")
        reason = (row.last_error or row.last_kind or "cooldown") if row is not None else "cooldown"
        return f"cursor-total circuit is open ({reason})"
    quota = snapshot if snapshot is not None else load_quota_snapshot(now=moment, allow_blocking=False)
    used = pool_pressure(quota, pool)
    if used >= HARD_THRESHOLD:
        hint = _reset_hint(quota, [pool], moment)
        return f"{pool} quota exhausted ({hint})" if hint else f"{pool} quota is at {used:.0f}%"
    return None


def admission_decision(
    *,
    prompt: str,
    state_id: str,
    role: str | None,
    specialist: str | None,
    attempt: int,
    pinned_model: str | None,
    pinned_thinking: str | None,
    exclude_models: tuple[str, ...] = (),
) -> RouteDecision | None:
    """A route that should change the spawn, or None to leave the state alone.

    Auto mode preserves a healthy pin on attempt 1, but proactively reroutes
    before spawning whenever the pinned pool is already HARD-exhausted (>= 98%)
    or circuit-open. Later attempts escalate and avoid exhausted pools, and a
    provider error reroutes through ``fallback_after_failure``. Explicit
    ``PA_MODEL_ROUTER=1`` (Active) also applies soft 90% steering and Switchyard
    on attempt 1. Unrecognized models, including factory test doubles, are never
    rewritten.
    """

    if router_disabled() or not is_recognized_model(pinned_model):
        return None
    explicit = router_explicit()
    snapshot = load_quota_snapshot(allow_blocking=False)
    health = get_router().health
    if not explicit and attempt <= 1 and pinned_model not in exclude_models:
        if pool_is_hard_exhausted(pinned_model, snapshot=snapshot, health=health) is None:
            return None
    decision = route_model(
        prompt=prompt,
        state_id=state_id,
        role=role,
        specialist=specialist,
        attempt=attempt,
        pinned_model=pinned_model,
        pinned_thinking=pinned_thinking,
        snapshot=snapshot,
        health=health,
        available=available_providers(),
        exclude_models=exclude_models,
        routing_profile=load_routing_profile(),
    )
    if decision.exhausted and not decision.model:
        return decision
    if not decision.model or decision.model == pinned_model:
        if (decision.thinking or "") == (pinned_thinking or ""):
            return None
    return decision


# The role model a factory step names to let the router pick its model at spawn time.
ROUTER_MODEL = "router"


def routed_step_decision(
    *,
    prompt: str,
    state_id: str,
    role: str | None,
    specialist: str | None,
    attempt: int,
    exclude_models: tuple[str, ...] = (),
) -> RouteDecision | None:
    """The model for a factory step that names ``ROUTER_MODEL``: the router picks from the
    step's task tier and the live quota. None when the router is off, so the step runs on
    the factory host's model."""

    if router_disabled():
        return None
    return route_model(
        prompt=prompt,
        state_id=state_id,
        role=role,
        specialist=specialist,
        attempt=attempt,
        snapshot=load_quota_snapshot(allow_blocking=False),
        health=get_router().health,
        available=available_providers(),
        exclude_models=exclude_models,
        routing_profile=load_routing_profile(),
    )


def fallback_after_failure(
    *,
    prompt: str,
    state_id: str,
    role: str | None,
    specialist: str | None,
    attempt: int,
    model: str,
    thinking: str | None,
    exclude_models: tuple[str, ...] = (),
) -> RouteDecision:
    """The next model after a provider auth or quota failure on ``model``."""

    snapshot = load_quota_snapshot(allow_blocking=False)
    excluded = tuple(dict.fromkeys((*exclude_models, model)))
    return route_model(
        prompt=prompt,
        state_id=state_id,
        role=role,
        specialist=specialist,
        attempt=max(1, attempt),
        pinned_model=model,
        pinned_thinking=thinking,
        snapshot=snapshot,
        health=get_router().health,
        available=available_providers(),
        exclude_models=excluded,
        routing_profile=load_routing_profile(),
    )


def format_captain_router_brief(
    *,
    snapshot: QuotaSnapshot | None = None,
    available: set[str] | None = None,
    health: Mapping[str, ProviderHealth] | None = None,
    now: float | None = None,
    spend_usd: float | None = None,
    input_tokens: int | None = None,
    output_tokens: int | None = None,
) -> str:
    """Compact quota, spend, and per-tier recommendation for the Captain."""

    moment = time.time() if now is None else now
    quota = snapshot if snapshot is not None else load_quota_snapshot(now=moment, allow_blocking=False)
    providers = available if available is not None else available_providers()
    scores = health if health is not None else get_router().health
    lines: list[str] = []
    if quota.error and not quota.pools:
        lines.append(f"Quota snapshot unavailable ({quota.error}).")
    elif quota.stale and quota.error:
        lines.append(f"Quota snapshot stale ({quota.error}).")
    pool_bits: list[str] = []
    for pool in (
        "claude",
        "codex",
        "antigravity-gemini",
        "antigravity-claude-gpt",
        "cursor-total",
        "cursor",
        "cursor-third-party",
    ):
        row = quota.pools.get(pool)
        if row is None:
            continue
        used = pool_pressure(quota, pool)
        window = row.binding_window()
        eta = _format_eta(window.resets_at, moment) if window is not None else "reset unknown"
        mark = ""
        if used >= HARD_THRESHOLD:
            mark = " HARD"
        elif used >= SOFT_THRESHOLD:
            mark = " soft"
        headroom = max(0.0, 100.0 - used)
        open_bit = " circuit-open" if _health_open(scores, pool, moment) else ""
        pool_bits.append(f"{pool} {used:.0f}% used, {headroom:.0f}% left, {eta}{mark}{open_bit}")
    if pool_bits:
        lines.append("Pools: " + "; ".join(pool_bits) + ".")
    recommendations: list[str] = []
    for tier in TIERS:
        pick = _recommend_tier(tier, quota, scores, providers, moment)
        if pick is None:
            recommendations.append(f"{tier}: none healthy")
        else:
            recommendations.append(f"{tier}: {pick.model} {pick.thinking}")
    if recommendations:
        lines.append("Recommend " + "; ".join(recommendations) + ".")
    active_profile = load_routing_profile()
    profile_line = f"Profile: {active_profile.slug} (priority={active_profile.provider_priority}"
    if active_profile.switchyard.enabled:
        profile_line += (
            f", switchyard={active_profile.switchyard.efficient_model}->"
            f"{active_profile.switchyard.capable_model} [{active_profile.switchyard.sensitivity}]"
        )
    profile_line += ")."
    gw = detect_ramp_router_gateway()
    if gw is not None:
        profile_line += f" Ramp Router gateway detected ({gw['source']})."
    lines.append(profile_line)
    lines.append(
        "Use fast or balanced for chores, docs, formatting, and summaries. "
        "Reserve frontier for architecture, deep math, hard debugging, and security audits. "
        "Reroute when a pool is at or above 90% for ordinary work, or 98% for frontier work."
    )
    if spend_usd is not None:
        tokens = ""
        if input_tokens is not None or output_tokens is not None:
            tokens = f" (in {int(input_tokens or 0)}, out {int(output_tokens or 0)})"
        lines.append(f"Project spend: ${float(spend_usd):.2f}{tokens}.")
    return "\n".join(lines)


def _recommend_tier(
    tier: str,
    snapshot: QuotaSnapshot,
    health: Mapping[str, ProviderHealth],
    available: set[str],
    now: float,
) -> ModelCandidate | None:
    rows = [
        row
        for row in CATALOG
        if row.tier == tier
        and row.provider in available
        and not _health_open(health, row.pool, now)
        and _pin_pressure_ok(snapshot, row.pool, tier)
    ]
    if not rows:
        return None
    return min(rows, key=lambda row: (row.cost, *_failure_score(health, row.pool), row.model))
