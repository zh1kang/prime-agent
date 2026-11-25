"""Quota, cost, and complexity routing for factory spawns and the Captain brief."""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import rlm as rlm_module
from rlm import model_router
from rlm.model_router import (
    ModelRouter,
    ProviderHealth,
    QuotaSnapshot,
    available_providers,
    classify_task,
    format_captain_router_brief,
    load_quota_snapshot,
    parse_quota_snapshot,
    pool_pressure,
    reset_router,
    route_model,
)
from test_factory import _ExecutorTestCase, async_test

NOW = 1_763_860_000.0  # 2026-11-23T04:00:00Z
RESET_SOON = "2026-11-23T08:00:00Z"
RESET_WEEK = "2026-11-30T00:00:00Z"

QUOTA_ANSWER = (
    "\n⚠ Individual quota reached. Please upgrade your subscription to increase "
    "your limits. Resets in 4h52m9s."
)
EXACT_PAUSE = (
    "Provider auth/quota error in state 'design': ⚠ Individual quota reached. Please upgrade "
    "your subscription to increase your limits. Resets in 4h52m9s."
)

# codexbar's real shape: null windows, split Cursor labels, Claude session/weekly, Codex weekly.
CODEXBAR_FIXTURE = """[
  {"provider":"codex","source":"oauth","rateWindowLabels":{"secondary":"Weekly"},
   "usage":{"primary":null,
     "secondary":{"usedPercent":100,"windowMinutes":10080,"resetsAt":"2026-11-26T03:59:42Z","resetDescription":"Nov 25 at 10:59 PM"},
     "tertiary":null}},
  {"provider":"claude","source":"web","rateWindowLabels":{"primary":"Session","secondary":"Weekly"},
   "usage":{"primary":{"usedPercent":16,"windowMinutes":300,"resetsAt":"2026-11-23T23:19:59Z"},
     "secondary":{"usedPercent":96,"windowMinutes":10080,"resetsAt":"2026-11-23T22:59:59Z"}}},
  {"provider":"cursor","source":"web","rateWindowLabels":{"primary":"Total","secondary":"Cursor","tertiary":"Third Party"},
   "usage":{"primary":{"usedPercent":17.3,"windowMinutes":43200},
     "secondary":{"usedPercent":1.5,"windowMinutes":43200},
     "tertiary":{"usedPercent":100,"windowMinutes":43200}}},
  {"provider":"antigravity","source":"cli",
   "usage":{"primary":{"usedPercent":25.73,"windowMinutes":300,"resetsAt":"2026-11-24T01:12:47Z"},
     "secondary":{"usedPercent":52.84,"windowMinutes":10080,"resetsAt":"2026-11-29T07:00:51Z"}},
   "error":null},
  {"provider":"zai","source":"api","usage":null,"error":{"code":1,"message":"Missing API key"}},
  {"source":"cli"}
]"""

SPLIT_ANTIGRAVITY = {
    "provider": "antigravity",
    "extraRateWindows": [
        {"label": "Gemini 5-hour", "usedPercent": 12, "windowMinutes": 300, "resetsAt": RESET_SOON},
        {"label": "Gemini weekly", "usedPercent": 20, "windowMinutes": 10080, "resetsAt": RESET_WEEK},
        {"label": "Claude/GPT 5-hour", "usedPercent": 96, "windowMinutes": 300, "resetsAt": RESET_SOON},
        {"label": "Claude/GPT weekly", "usedPercent": 30, "windowMinutes": 10080, "resetsAt": RESET_WEEK},
    ],
    "usage": {
        "primary": {"usedPercent": 99, "windowMinutes": 300, "resetsAt": RESET_SOON},
        "secondary": {"usedPercent": 99, "windowMinutes": 10080, "resetsAt": RESET_WEEK},
    },
}


def _providers(*names: str) -> set[str]:
    return set(names)


class ParseQuotaTest(unittest.TestCase):
    def test_real_codexbar_windows_and_nulls(self) -> None:
        snapshot = parse_quota_snapshot(CODEXBAR_FIXTURE, now=NOW)
        self.assertEqual(snapshot.error, None)
        self.assertAlmostEqual(pool_pressure(snapshot, "codex"), 100.0)
        self.assertAlmostEqual(pool_pressure(snapshot, "claude"), 96.0)
        self.assertAlmostEqual(snapshot.pools["cursor-total"].used_percent, 17.3)
        self.assertAlmostEqual(snapshot.pools["cursor"].used_percent, 1.5)
        self.assertAlmostEqual(pool_pressure(snapshot, "cursor"), 17.3)
        self.assertAlmostEqual(pool_pressure(snapshot, "cursor-third-party"), 100.0)
        # Unlabeled Antigravity windows apply to both of its pools.
        self.assertAlmostEqual(pool_pressure(snapshot, "antigravity-gemini"), 52.84)
        self.assertAlmostEqual(pool_pressure(snapshot, "antigravity-claude-gpt"), 52.84)
        self.assertNotIn("zai", snapshot.pools)
        self.assertIn("codex", snapshot.providers)

    def test_antigravity_extra_windows_split_gemini_from_claude_gpt(self) -> None:
        snapshot = parse_quota_snapshot([SPLIT_ANTIGRAVITY], now=NOW)
        self.assertAlmostEqual(pool_pressure(snapshot, "antigravity-gemini"), 20.0)
        self.assertAlmostEqual(pool_pressure(snapshot, "antigravity-claude-gpt"), 96.0)
        gemini = snapshot.pools["antigravity-gemini"].binding_window()
        self.assertIsNotNone(gemini)
        assert gemini is not None
        self.assertEqual(gemini.label, "Gemini weekly")
        self.assertGreater(gemini.resets_at or 0, NOW)

    def test_extra_windows_as_a_dict(self) -> None:
        payload = {
            "provider": "antigravity",
            "extraRateWindows": {
                "Gemini 5-hour": {"usedPercent": 4, "resetsAt": RESET_SOON},
                "Claude/GPT weekly": 91,
            },
        }
        snapshot = parse_quota_snapshot(payload, now=NOW)
        self.assertAlmostEqual(pool_pressure(snapshot, "antigravity-gemini"), 4.0)
        self.assertAlmostEqual(pool_pressure(snapshot, "antigravity-claude-gpt"), 91.0)

    def test_real_codexbar_nested_extra_windows_split_gemini(self) -> None:
        # codexbar usage --json-only nests extra windows under usage, each with a title
        # and a nested window. The unlabeled primary is the Claude/GPT quota.
        payload = [
            {
                "provider": "antigravity",
                "source": "cli",
                "usage": {
                    "primary": {"usedPercent": 100, "windowMinutes": 300, "resetsAt": RESET_SOON},
                    "secondary": None,
                    "extraRateWindows": [
                        {
                            "id": "gemini-5h",
                            "title": "Gemini 5-hour",
                            "window": {"usedPercent": 12.5, "windowMinutes": 300, "resetsAt": RESET_SOON},
                        },
                        {
                            "id": "gemini-weekly",
                            "title": "Gemini weekly",
                            "window": {"usedPercent": 31, "windowMinutes": 10080, "resetsAt": RESET_WEEK},
                        },
                    ],
                },
                "error": None,
            }
        ]
        snapshot = parse_quota_snapshot(payload, now=NOW)
        gemini = snapshot.pools["antigravity-gemini"]
        labels = {window.label: window for window in gemini.windows}
        self.assertEqual(set(labels), {"Gemini 5-hour", "Gemini weekly"})
        self.assertAlmostEqual(labels["Gemini 5-hour"].used_percent, 12.5)
        self.assertEqual(labels["Gemini 5-hour"].window_minutes, 300)
        self.assertGreater(labels["Gemini 5-hour"].resets_at or 0, NOW)
        self.assertAlmostEqual(pool_pressure(snapshot, "antigravity-gemini"), 31.0)
        # The full unlabeled window does not mark Gemini exhausted.
        self.assertAlmostEqual(pool_pressure(snapshot, "antigravity-claude-gpt"), 100.0)

    def test_garbage_is_an_empty_snapshot(self) -> None:
        snapshot = parse_quota_snapshot("not json", now=NOW)
        self.assertEqual(snapshot.pools, {})
        self.assertIn("unreadable", snapshot.error or "")
        self.assertEqual(parse_quota_snapshot("42", now=NOW).error, "unexpected usage output")


class ClassifyTaskTest(unittest.TestCase):
    def test_chores_docs_and_summaries_stay_fast(self) -> None:
        for prompt in (
            "fix the typo in the readme",
            "format this file",
            "write a changelog summary",
            "rename the helper and update the comment",
        ):
            profile = classify_task(prompt=prompt, state_id="docs", attempt=1)
            self.assertEqual(profile.tier, "fast", prompt)
            self.assertEqual(profile.thinking, "low")

    def test_architecture_math_and_audits_are_frontier(self) -> None:
        cases = (
            ("Design the architecture of the router.", "design", None),
            ("Prove the algorithm's complexity bound.", "work", None),
            ("Run a security audit and threat model.", "audit", "security"),
            ("Find the root cause of this race.", "debug", "auditor"),
        )
        for prompt, state_id, role in cases:
            profile = classify_task(prompt=prompt, state_id=state_id, role=role)
            self.assertEqual(profile.tier, "frontier", prompt)
            self.assertEqual(profile.thinking, "xhigh")

    def test_retry_escalates_one_tier_at_a_time(self) -> None:
        prompt = "fix the typo in the changelog"
        self.assertEqual(classify_task(prompt=prompt, attempt=1).tier, "fast")
        second = classify_task(prompt=prompt, attempt=2)
        self.assertEqual(second.tier, "balanced")
        self.assertEqual(second.escalated_from, "fast")
        self.assertEqual(second.thinking, "medium")
        self.assertEqual(classify_task(prompt=prompt, attempt=3).tier, "strong")
        self.assertEqual(classify_task(prompt=prompt, attempt=6).tier, "frontier")

    def test_a_role_can_raise_a_small_prompt(self) -> None:
        profile = classify_task(prompt="look at this", role="architect")
        self.assertEqual(profile.tier, "frontier")


class RouteDecisionTest(unittest.TestCase):
    def setUp(self) -> None:
        self.available = _providers("claude", "codex", "antigravity", "cursor")

    def _snapshot(self, pools: dict[str, float]) -> QuotaSnapshot:
        rows = []
        for pool, used in pools.items():
            provider = {
                "claude": "claude",
                "codex": "codex",
                "antigravity-gemini": "antigravity",
                "antigravity-claude-gpt": "antigravity",
                "cursor": "cursor",
                "cursor-total": "cursor",
                "cursor-third-party": "cursor",
            }[pool]
            label = {
                "claude": "Weekly",
                "codex": "Weekly",
                "antigravity-gemini": "Gemini weekly",
                "antigravity-claude-gpt": "Claude/GPT weekly",
                "cursor": "Cursor",
                "cursor-total": "Total",
                "cursor-third-party": "Third Party",
            }[pool]
            rows.append(
                {
                    "provider": provider,
                    "extraRateWindows": [
                        {"label": label, "usedPercent": used, "resetsAt": RESET_SOON},
                    ]
                    if provider == "antigravity"
                    else None,
                    "rateWindowLabels": {"primary": label},
                    "usage": {"primary": {"usedPercent": used, "resetsAt": RESET_SOON}},
                }
            )
        return parse_quota_snapshot(rows, now=NOW)

    def test_a_healthy_pin_is_kept(self) -> None:
        decision = route_model(
            prompt="Design the architecture.",
            pinned_model="anthropic/claude-opus-5-5",
            pinned_thinking="high",
            snapshot=self._snapshot({"claude": 10, "codex": 10}),
            available=self.available,
            now=NOW,
        )
        self.assertEqual(decision.model, "anthropic/claude-opus-5-5")
        self.assertFalse(decision.rerouted)
        self.assertEqual(decision.thinking, "high")
        self.assertFalse(decision.exhausted)

    def test_soft_threshold_steers_fast_work_off_a_near_full_pool(self) -> None:
        decision = route_model(
            prompt="fix the typo in the readme",
            state_id="docs",
            pinned_model="anthropic/claude-opus-5-5",
            snapshot=self._snapshot({"claude": 92, "codex": 5, "antigravity-gemini": 4, "cursor": 3}),
            available=self.available,
            now=NOW,
        )
        self.assertTrue(decision.rerouted)
        self.assertNotEqual(decision.model, "anthropic/claude-opus-5-5")
        self.assertEqual(decision.tier, "fast")
        self.assertEqual(decision.model, "openai-codex/gpt-5.4-mini")
        self.assertIn("claude", decision.reset_hint)

    def test_hard_threshold_reroutes_frontier_work(self) -> None:
        decision = route_model(
            prompt="security audit and threat model",
            pinned_model="anthropic/claude-opus-5-5",
            snapshot=self._snapshot(
                {"claude": 99, "codex": 12, "antigravity-gemini": 10, "antigravity-claude-gpt": 99}
            ),
            available=self.available,
            now=NOW,
        )
        self.assertTrue(decision.rerouted)
        self.assertEqual(decision.tier, "frontier")
        self.assertEqual(decision.model, "antigravity/gemini-4-neon")
        self.assertNotIn("claude-opus", decision.model)

    def test_frontier_pin_under_the_soft_line_but_under_hard_stays(self) -> None:
        decision = route_model(
            prompt="security audit and threat model",
            pinned_model="anthropic/claude-opus-5-5",
            snapshot=self._snapshot({"claude": 92, "codex": 1}),
            available=self.available,
            now=NOW,
        )
        self.assertFalse(decision.rerouted)
        self.assertEqual(decision.model, "anthropic/claude-opus-5-5")

    def test_every_pool_exhausted_names_the_reset(self) -> None:
        decision = route_model(
            prompt="security audit and threat model",
            pinned_model="anthropic/claude-opus-5-5",
            snapshot=self._snapshot({"claude": 99}),
            available=_providers("claude"),
            now=NOW,
        )
        self.assertTrue(decision.exhausted)
        self.assertEqual(decision.model, "")
        self.assertIn("No healthy fallback", decision.reason)
        self.assertIn("claude", decision.reset_hint)
        self.assertIn("resets in", decision.reset_hint)

    def test_escalation_prefers_the_same_pool(self) -> None:
        decision = route_model(
            prompt="fix the typo in the readme",
            state_id="docs",
            attempt=2,
            pinned_model="antigravity/gemini-3.8-flash",
            snapshot=self._snapshot({"antigravity-gemini": 10, "codex": 1, "claude": 1}),
            available=self.available,
            now=NOW,
        )
        self.assertEqual(decision.tier, "balanced")
        self.assertEqual(decision.model, "antigravity/gemini-4-argon")

    def test_open_circuit_skips_the_pool_until_cooldown(self) -> None:
        home = TemporaryDirectory()
        self.addCleanup(home.cleanup)
        router = ModelRouter(home=Path(home.name))
        router.record_failure("claude", "429 rate limit reached", now=NOW)
        self.assertTrue(router.pool_open("claude", NOW + 30))
        self.assertFalse(router.pool_open("claude", NOW + RATE_WAIT))
        blocked = route_model(
            prompt="security audit and threat model",
            pinned_model="anthropic/claude-opus-5-5",
            snapshot=self._snapshot({"claude": 10, "codex": 10}),
            health=router.health,
            available=self.available,
            now=NOW + 30,
        )
        self.assertTrue(blocked.rerouted)
        self.assertNotEqual(blocked.model.split("/")[0], "anthropic")
        recovered = route_model(
            prompt="security audit and threat model",
            pinned_model="anthropic/claude-opus-5-5",
            pinned_thinking="high",
            snapshot=self._snapshot({"claude": 10, "codex": 10}),
            health=router.health,
            available=self.available,
            now=NOW + RATE_WAIT,
        )
        self.assertEqual(recovered.model, "anthropic/claude-opus-5-5")
        self.assertFalse(recovered.rerouted)

    def test_ewma_moves_toward_the_new_sample(self) -> None:
        home = TemporaryDirectory()
        self.addCleanup(home.cleanup)
        router = ModelRouter(home=Path(home.name))
        router.record_failure("codex", "quota exceeded", now=NOW, reset_at=NOW + 3600)
        self.assertEqual(router.health["codex"].failure_ewma, 1.0)
        self.assertGreater(router.health["codex"].open_until, NOW + 3000)
        router.record_success("codex", 400, now=NOW + 10)
        self.assertAlmostEqual(router.health["codex"].failure_ewma, 0.8)
        self.assertAlmostEqual(router.health["codex"].latency_ewma_ms, 400.0)
        stored = json.loads((Path(home.name) / "router-health.json").read_text(encoding="utf-8"))
        self.assertIn("codex", stored["pools"])


RATE_WAIT = model_router.RATE_LIMIT_COOLDOWN_SECONDS


class PinnedTierAndQuotaAwareTest(unittest.TestCase):
    def setUp(self) -> None:
        self.available = _providers("claude", "codex", "antigravity", "cursor")

    def _snapshot(self, pools: dict[str, float]) -> QuotaSnapshot:
        return RouteDecisionTest._snapshot(self, pools)  # type: ignore[arg-type]

    def test_exhausted_frontier_pin_keeps_its_tier_on_balanced_work(self) -> None:
        snapshot = self._snapshot({"codex": 99, "claude": 10, "antigravity-gemini": 10, "cursor": 5})
        admission = route_model(
            prompt="Add the parser.",
            state_id="implement",
            pinned_model="openai-codex/gpt-5.5",
            snapshot=snapshot,
            available=self.available,
            now=NOW,
        )
        self.assertTrue(admission.rerouted)
        self.assertEqual(admission.tier, "frontier")
        self.assertEqual(admission.model, "antigravity/gemini-4-neon")
        self.assertEqual(admission.thinking, "xhigh")

        healthy = self._snapshot({"codex": 10, "claude": 10, "antigravity-gemini": 10, "cursor": 5})
        after_failure = route_model(
            prompt="Add the parser.",
            state_id="implement",
            pinned_model="openai-codex/gpt-5.5",
            snapshot=healthy,
            available=self.available,
            exclude_models=("openai-codex/gpt-5.5",),
            now=NOW,
        )
        self.assertEqual(after_failure.tier, "frontier")
        self.assertNotEqual(after_failure.model, "cursor/composer-2.5")
        self.assertEqual(after_failure.model, "openai-codex/gpt-6.1-sol")

    def test_soft_steering_may_still_move_a_pin_to_a_cheaper_tier(self) -> None:
        decision = route_model(
            prompt="Add the parser.",
            state_id="implement",
            pinned_model="openai-codex/gpt-5.5",
            snapshot=self._snapshot({"codex": 93, "claude": 10, "antigravity-gemini": 10, "cursor": 5}),
            available=self.available,
            now=NOW,
        )
        self.assertTrue(decision.rerouted)
        self.assertEqual(decision.tier, "balanced")

    def test_quota_aware_prefers_headroom_before_cost(self) -> None:
        snapshot = self._snapshot(
            {"codex": 60, "claude": 5, "antigravity-gemini": 70, "antigravity-claude-gpt": 70, "cursor": 80}
        )
        decision = route_model(
            prompt="Add the parser.",
            state_id="implement",
            snapshot=snapshot,
            available=self.available,
            now=NOW,
            routing_profile=model_router.RoutingProfile(provider_priority="quota_aware"),
        )
        self.assertEqual(decision.model, "anthropic/claude-sonnet-5-5")
        cheapest = route_model(
            prompt="Add the parser.",
            state_id="implement",
            snapshot=snapshot,
            available=self.available,
            now=NOW,
            routing_profile=model_router.RoutingProfile(provider_priority="favor_cost"),
        )
        self.assertEqual(cheapest.model, "cursor/composer-2.5")


class ProviderHealthTest(unittest.TestCase):
    def test_relative_reset_durations(self) -> None:
        cases = {
            "Resets in 4h52m9s.": 4 * 3600 + 52 * 60 + 9,
            "Resets in 59m9s.": 59 * 60 + 9,
            "quota reached; resets in 45m": 45 * 60,
            "Resets in 0h59m9s": 59 * 60 + 9,
            "Your limit resets in 4 days, 9 hours.": 4 * 86400 + 9 * 3600,
            "resets in 4d 9h": 4 * 86400 + 9 * 3600,
            "Rate limit reached. Please try again in 20s.": 20,
            "Resets in 4h.": 4 * 3600,
        }
        for text, seconds in cases.items():
            self.assertEqual(model_router._relative_reset(text, NOW), NOW + seconds, text)
        for text in ("You've hit your limit · resets 2am", "resets Nov 25 at 10:59 PM", "quota exceeded"):
            self.assertIsNone(model_router._relative_reset(text, NOW), text)

    def test_error_reset_wins_over_a_far_weekly_window(self) -> None:
        # Gemini's weekly window resets in days but is only half used; the error
        # names the real reset, so the circuit must not stay open for days.
        snapshot = parse_quota_snapshot(
            [
                {
                    "provider": "antigravity",
                    "extraRateWindows": [
                        {"label": "Gemini 5-hour", "usedPercent": 100, "resetsAt": RESET_SOON},
                        {"label": "Gemini weekly", "usedPercent": 99.5, "resetsAt": RESET_WEEK},
                    ],
                }
            ],
            now=NOW,
        )
        with TemporaryDirectory() as tmp:
            router = ModelRouter(home=Path(tmp))
            router.record_model_failure(
                "antigravity/gemini-4-neon",
                "Individual quota reached. Resets in 59m9s.",
                snapshot=snapshot,
                now=NOW,
            )
            self.assertEqual(router.health["antigravity-gemini"].open_until, NOW + 59 * 60 + 9)

    def test_snapshot_reset_comes_only_from_an_exhausted_window(self) -> None:
        snapshot = parse_quota_snapshot(
            [
                {
                    "provider": "antigravity",
                    "extraRateWindows": [
                        {"label": "Gemini 5-hour", "usedPercent": 100, "resetsAt": RESET_SOON},
                        {"label": "Gemini weekly", "usedPercent": 52, "resetsAt": RESET_WEEK},
                    ],
                }
            ],
            now=NOW,
        )
        soon = model_router._parse_time(RESET_SOON)
        with TemporaryDirectory() as tmp:
            router = ModelRouter(home=Path(tmp))
            router.record_model_failure("antigravity/gemini-4-neon", "quota exceeded", snapshot=snapshot, now=NOW)
            self.assertEqual(router.health["antigravity-gemini"].open_until, soon)

            calm = parse_quota_snapshot(
                [{"provider": "antigravity", "extraRateWindows": [{"label": "Gemini weekly", "usedPercent": 52, "resetsAt": RESET_WEEK}]}],
                now=NOW,
            )
            router.record_model_failure("antigravity/gemini-4-neon", "quota exceeded", snapshot=calm, now=NOW)
            self.assertEqual(
                router.health["antigravity-gemini"].open_until, NOW + model_router.QUOTA_COOLDOWN_SECONDS
            )

    def test_two_routers_keep_each_others_pools_on_disk(self) -> None:
        with TemporaryDirectory() as tmp:
            home = Path(tmp)
            factory_side = ModelRouter(home=home)
            captain_side = ModelRouter(home=home)
            # Both loaded an empty file; each writes a different pool afterwards.
            factory_side.record_failure("claude", "quota exceeded", now=NOW, reset_at=NOW + 3600)
            captain_side.record_failure("codex", "429 rate limit reached", now=NOW)
            factory_side.record_success("cursor", 250, now=NOW)
            stored = json.loads((home / "router-health.json").read_text(encoding="utf-8"))
            self.assertEqual(set(stored["pools"]), {"claude", "codex", "cursor"})
            self.assertEqual(stored["pools"]["claude"]["open_until"], NOW + 3600)
            self.assertEqual(stored["pools"]["codex"]["last_kind"], "rate_limit")
            self.assertTrue((home / "router-health.lock").exists())
            self.assertEqual(list(home.glob("*.tmp")), [])

    def test_note_provider_success_decays_the_failure_score(self) -> None:
        with TemporaryDirectory() as tmp:
            with patch.dict(os.environ, {"PA_ROUTER_HOME": tmp, "PA_MODEL_ROUTER": "auto"}, clear=False):
                reset_router()
                self.addCleanup(reset_router)
                model_router.get_router().record_failure("codex", "quota exceeded", now=NOW)
                model_router.note_provider_success("openai-codex/gpt-5.5", 1200, now=NOW + 5)
                model_router.note_provider_success("pi/test-model", 50, now=NOW + 5)
                row = model_router.get_router().health["codex"]
                self.assertAlmostEqual(row.failure_ewma, 0.8)
                self.assertAlmostEqual(row.latency_ewma_ms, 1200.0)
                self.assertNotIn("pi", model_router.get_router().health)
            with patch.dict(os.environ, {"PA_ROUTER_HOME": tmp, "PA_MODEL_ROUTER": "0"}, clear=False):
                reset_router()
                model_router.note_provider_success("openai-codex/gpt-5.5", 1200, now=NOW + 6)
                self.assertEqual(model_router.get_router().health["codex"].samples, 2)


class QuotaCacheTest(unittest.TestCase):
    def setUp(self) -> None:
        reset_router()
        self.addCleanup(reset_router)

    def test_fresh_cache_skips_the_process_and_errors_fall_back_to_stale(self) -> None:
        root = TemporaryDirectory()
        self.addCleanup(root.cleanup)
        path = Path(root.name) / "router-quota-cache.json"
        raw = json.dumps(
            [{"provider": "codex", "rateWindowLabels": {"primary": "Weekly"}, "usage": {"primary": {"usedPercent": 7}}}]
        )
        calls: list[float] = []

        def runner(timeout: float) -> str:
            calls.append(timeout)
            return raw

        first = load_quota_snapshot(now=5_000, runner=runner, cache_path=path, ttl=60, timeout=2.5)
        second = load_quota_snapshot(now=5_030, runner=runner, cache_path=path, ttl=60, timeout=2.5)
        self.assertEqual(calls, [2.5])
        self.assertAlmostEqual(second.pools["codex"].used_percent, 7.0)
        self.assertFalse(second.stale)
        self.assertEqual(first.pools["codex"].used_percent, 7.0)

        def fail(_timeout: float) -> str:
            raise RuntimeError("codexbar down")

        stale = load_quota_snapshot(now=5_200, runner=fail, cache_path=path, ttl=60)
        self.assertTrue(stale.stale)
        self.assertAlmostEqual(stale.pools["codex"].used_percent, 7.0)
        self.assertIn("codexbar down", stale.error or "")

    def test_timeout_does_not_raise(self) -> None:
        root = TemporaryDirectory()
        self.addCleanup(root.cleanup)
        path = Path(root.name) / "cache.json"

        def hang(_timeout: float) -> str:
            raise subprocess.TimeoutExpired(cmd="codexbar", timeout=2.5)

        snapshot = load_quota_snapshot(now=NOW, runner=hang, cache_path=path, timeout=2.5)
        self.assertIsInstance(snapshot, QuotaSnapshot)
        self.assertIn("timed out", snapshot.error or "")
        self.assertEqual(snapshot.pools, {})

    def test_stale_disk_cache_returns_immediately_without_blocking_codexbar(self) -> None:
        root = TemporaryDirectory()
        self.addCleanup(root.cleanup)
        path = Path(root.name) / "router-quota-cache.json"
        raw = json.dumps(
            [{"provider": "claude", "rateWindowLabels": {"primary": "Session"}, "usage": {"primary": {"usedPercent": 42}}}]
        )
        path.write_text(json.dumps({"fetched_at": 1000.0, "raw": raw}), encoding="utf-8")
        spawned: list[Path] = []
        with patch.object(model_router, "_spawn_background_quota_refresh", side_effect=lambda p, _t: spawned.append(p)):
            with patch.object(model_router, "_run_codexbar", side_effect=AssertionError("must not block")):
                snap = load_quota_snapshot(now=5000.0, cache_path=path, ttl=60.0)
        self.assertTrue(snap.stale)
        self.assertAlmostEqual(snap.pools["claude"].used_percent, 42.0)
        self.assertEqual(spawned, [path])

        empty_path = Path(root.name) / "missing-cache.json"
        spawned.clear()
        with patch.object(model_router, "_spawn_background_quota_refresh", side_effect=lambda p, _t: spawned.append(p)):
            with patch.object(model_router, "_run_codexbar", side_effect=AssertionError("must not block")):
                cold = load_quota_snapshot(now=5000.0, cache_path=empty_path, ttl=60.0, allow_blocking=False)
        self.assertEqual(cold.pools, {})
        self.assertEqual(spawned, [empty_path])


class BriefTest(unittest.TestCase):
    def test_brief_names_headroom_and_a_model_per_tier(self) -> None:
        snapshot = parse_quota_snapshot(CODEXBAR_FIXTURE, now=NOW)
        text = format_captain_router_brief(
            snapshot=snapshot,
            available=_providers("claude", "codex", "antigravity", "cursor"),
            health={},
            now=NOW,
            spend_usd=12.5,
            input_tokens=1000,
            output_tokens=250,
        )
        self.assertIn("claude 96% used", text)
        self.assertIn("soft", text)
        self.assertIn("codex 100% used", text)
        self.assertIn("HARD", text)
        self.assertIn("cursor-third-party 100% used", text)
        self.assertIn("frontier: antigravity/gemini-4-neon xhigh", text)
        self.assertIn("fast: antigravity/gemini-3.8-flash low", text)
        self.assertIn("Project spend: $12.50 (in 1000, out 250).", text)
        self.assertIn("Reserve frontier", text)


class AvailabilityTest(unittest.TestCase):
    def test_provider_override_replaces_detection(self) -> None:
        with patch.dict(os.environ, {"PA_ROUTER_PROVIDERS": "codex, cursor"}, clear=False):
            self.assertEqual(available_providers(), {"codex", "cursor"})
        with patch.dict(os.environ, {"PA_ROUTER_PROVIDERS": ""}, clear=False):
            self.assertEqual(available_providers(), set())


class RouterFactoryTest(_ExecutorTestCase):
    """Admission and quota-error integration. Unrecognized models stay put."""

    def _isolate(self, updates: dict[str, str], remove: tuple[str, ...] = ()) -> None:
        keys = list(updates) + list(remove)
        previous = {key: os.environ.get(key) for key in keys}

        def restore() -> None:
            for key, value in previous.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
            reset_router()

        self.addCleanup(restore)
        for key in remove:
            os.environ.pop(key, None)
        os.environ.update(updates)
        reset_router()

    def _machine(self, model: str, prompt: str = "Design the architecture of the quota router.") -> None:
        self.store_machine(
            {
                "states": [
                    {
                        "id": "design",
                        "entry": True,
                        "subagent": {"prompt": prompt, "model": model},
                    }
                ],
                "transitions": [],
            }
        )

    def _quota(self, *, gemini: float, claude_gpt: float, codex: float) -> str:
        return json.dumps(
            [
                {
                    "provider": "antigravity",
                    "extraRateWindows": [
                        {"label": "Gemini weekly", "usedPercent": gemini, "resetsAt": RESET_SOON},
                        {"label": "Claude/GPT weekly", "usedPercent": claude_gpt, "resetsAt": RESET_WEEK},
                    ],
                },
                {
                    "provider": "codex",
                    "rateWindowLabels": {"primary": "Weekly"},
                    "usage": {"primary": {"usedPercent": codex, "resetsAt": RESET_SOON}},
                },
            ]
        )

    @async_test
    async def test_quota_error_reroutes_to_the_next_healthy_pool(self) -> None:
        home = TemporaryDirectory()
        self.addCleanup(home.cleanup)
        self._isolate(
            {
                "PA_ROUTER_HOME": home.name,
                "PA_ROUTER_PROVIDERS": "antigravity,codex",
                "PA_ROUTER_QUOTA_JSON": self._quota(gemini=10, claude_gpt=10, codex=5),
                "PA_CODEXBAR_BIN": "none",
            },
            remove=("PA_MODEL_ROUTER",),
        )
        self._machine("antigravity/claude-opus-5-5")
        self.host.outcomes["design"] = {"status": "done", "answer": QUOTA_ANSWER}
        gate = self.host.gate("rlm.collect", 2)
        started = await self.start()
        await asyncio.wait_for(self.host.gate_entered("rlm.collect", 2).wait(), 2)
        self.host.outcomes["design"] = {"status": "done", "answer": "architecture is sound"}
        gate.set()
        await self.executor._runs[started["run_id"]].task
        status = await rlm_module.rlm.factory.status(started["run_id"])

        self.assertEqual(status["state"], "done", status.get("pause_reason") or status.get("last_error"))
        models = [call["kwargs"].get("model") for call in self.host.spawn_calls("design")]
        self.assertEqual(models, ["antigravity/claude-opus-5-5", "antigravity/gemini-4-neon"])
        # State pin stays intact across instance-scoped provider reroutes (I1).
        self.assertEqual(self.executor._runs[started["run_id"]].states["design"].model, "antigravity/claude-opus-5-5")
        rerouted = [event for event in status["events"] if event["kind"] == "model_rerouted"]
        self.assertEqual(len(rerouted), 1)
        self.assertIn("antigravity/claude-opus-5-5", rerouted[0]["detail"])
        self.assertIn("antigravity/gemini-4-neon", rerouted[0]["detail"])

    @async_test
    async def test_auto_mode_proactively_reroutes_hard_exhausted_pool_before_spawn(self) -> None:
        home = TemporaryDirectory()
        self.addCleanup(home.cleanup)
        quota = json.dumps(
            [
                {
                    "provider": "claude",
                    "rateWindowLabels": {"primary": "Session", "secondary": "Weekly"},
                    "usage": {
                        "primary": {"usedPercent": 100, "resetsAt": RESET_SOON},
                        "secondary": {"usedPercent": 100, "resetsAt": RESET_WEEK},
                    },
                },
                {
                    "provider": "antigravity",
                    "extraRateWindows": [
                        {"label": "Gemini weekly", "usedPercent": 10, "resetsAt": RESET_SOON},
                        {"label": "Claude/GPT weekly", "usedPercent": 99, "resetsAt": RESET_WEEK},
                    ],
                },
            ]
        )
        self._isolate(
            {
                "PA_ROUTER_HOME": home.name,
                "PA_ROUTER_PROVIDERS": "claude,antigravity",
                "PA_ROUTER_QUOTA_JSON": quota,
                "PA_CODEXBAR_BIN": "none",
            },
            remove=("PA_MODEL_ROUTER",),
        )
        self._machine("anthropic/claude-opus-5-5")
        self.host.outcomes["design"] = {"status": "done", "answer": "architecture is sound"}
        started = await self.start()
        await self.executor._runs[started["run_id"]].task
        status = await rlm_module.rlm.factory.status(started["run_id"])

        self.assertEqual(status["state"], "done", status.get("pause_reason") or status.get("last_error"))
        models = [call["kwargs"].get("model") for call in self.host.spawn_calls("design")]
        self.assertEqual(models, ["antigravity/gemini-4-neon"])
        rerouted = [event for event in status["events"] if event["kind"] == "model_rerouted"]
        self.assertEqual(len(rerouted), 1)
        self.assertIn("anthropic/claude-opus-5-5 -> antigravity/gemini-4-neon", rerouted[0]["detail"])

    @async_test
    async def test_hidden_quota_behind_missing_output_reroutes_instead_of_pausing(self) -> None:
        home = TemporaryDirectory()
        self.addCleanup(home.cleanup)
        sessions = TemporaryDirectory()
        self.addCleanup(sessions.cleanup)
        self.host.session_root = Path(sessions.name)
        self._isolate(
            {
                "PA_ROUTER_HOME": home.name,
                "PA_ROUTER_PROVIDERS": "antigravity,codex",
                "PA_ROUTER_QUOTA_JSON": self._quota(gemini=10, claude_gpt=10, codex=99),
                "PA_CODEXBAR_BIN": "none",
            },
            remove=("PA_MODEL_ROUTER",),
        )
        self.store_machine(
            {
                "states": [
                    {
                        "id": "design",
                        "entry": True,
                        "subagent": {
                            "prompt": "Design the architecture of the quota router.",
                            "model": "antigravity/claude-opus-5-5",
                        },
                        "outputs": [{"name": "summary", "type": "text"}],
                        "emit": {"outputs": [{"name": "summary", "type": "text"}], "records": {}},
                    }
                ],
                "transitions": [],
            }
        )
        # First child settles with partial prose (no factory-outputs) while its session
        # JSONL records a provider_retry_outcome quota banner from pa-daemon.
        first_answer = "I'll start by checking the existing files."
        self.host.outcomes["design"] = {"status": "done", "answer": first_answer}
        gate = self.host.gate("rlm.collect", 1)
        started = await self.start()
        await asyncio.wait_for(self.host.gate_entered("rlm.collect", 1).wait(), 2)
        run = self.executor._runs[started["run_id"]]
        inst = run.states["design"].entries[0].instances[0]
        assert inst.session_dir is not None
        (Path(inst.session_dir) / "session.jsonl").write_text(
            json.dumps(
                {
                    "type": "custom_message",
                    "customType": "provider_retry_outcome",
                    "content": "Retried 8 times without recovery",
                    "display": True,
                    "details": {
                        "success": False,
                        "attempts": 8,
                        "finalError": (
                            "⚠ Individual quota reached. "
                            "Please upgrade your subscription to increase your limits. Resets in 4h."
                        ),
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        # Second attempt (after automatic reroute to gemini-4-neon) emits the required output.
        gate2 = self.host.gate("rlm.collect", 2)
        gate.set()
        await asyncio.wait_for(self.host.gate_entered("rlm.collect", 2).wait(), 2)
        self.host.outcomes["design"] = {
            "status": "done",
            "answer": 'Done.\n\n```factory-outputs\n{"summary": "implemented cleanly"}\n```',
        }
        gate2.set()
        await run.task
        status = await rlm_module.rlm.factory.status(started["run_id"])

        self.assertEqual(status["state"], "done", status.get("pause_reason") or status.get("last_error"))
        models = [call["kwargs"].get("model") for call in self.host.spawn_calls("design")]
        self.assertEqual(models, ["antigravity/claude-opus-5-5", "antigravity/gemini-4-neon"])
        rerouted = [event for event in status["events"] if event["kind"] == "model_rerouted"]
        self.assertEqual(len(rerouted), 1)

    @async_test
    async def test_quota_error_pauses_when_every_pool_is_exhausted(self) -> None:
        home = TemporaryDirectory()
        self.addCleanup(home.cleanup)
        self._isolate(
            {
                "PA_ROUTER_HOME": home.name,
                "PA_ROUTER_PROVIDERS": "antigravity",
                "PA_ROUTER_QUOTA_JSON": self._quota(gemini=99, claude_gpt=10, codex=0),
                "PA_CODEXBAR_BIN": "none",
            },
            remove=("PA_MODEL_ROUTER",),
        )
        self._machine("antigravity/claude-opus-5-5")
        self.host.outcomes["design"] = {"status": "done", "answer": QUOTA_ANSWER}
        started = await self.start()
        await self.executor._runs[started["run_id"]].task
        status = await rlm_module.rlm.factory.status(started["run_id"])

        self.assertEqual(status["state"], "paused")
        self.assertIn(EXACT_PAUSE, status["pause_reason"])
        self.assertIn("No healthy fallback", status["pause_reason"])
        self.assertIn("antigravity-gemini", status["pause_reason"])
        self.assertIn("resets in", status["pause_reason"])
        self.assertEqual(len(self.host.spawn_calls("design")), 1)
        self.assertNotIn("model_rerouted", [event["kind"] for event in status["events"]])

    @async_test
    async def test_explicit_router_pauses_before_spawn_when_all_pools_are_hard_full(self) -> None:
        home = TemporaryDirectory()
        self.addCleanup(home.cleanup)
        self._isolate(
            {
                "PA_ROUTER_HOME": home.name,
                "PA_MODEL_ROUTER": "1",
                "PA_ROUTER_PROVIDERS": "antigravity",
                "PA_ROUTER_QUOTA_JSON": self._quota(gemini=99, claude_gpt=99, codex=0),
                "PA_CODEXBAR_BIN": "none",
            }
        )
        self._machine("antigravity/claude-opus-5-5")
        started = await self.start()
        status = await rlm_module.rlm.factory.status(started["run_id"])

        self.assertEqual(status["state"], "paused", status)
        self.assertIn("No healthy fallback", status["pause_reason"])
        self.assertIn("antigravity-gemini", status["pause_reason"])
        self.assertIn("antigravity-claude-gpt", status["pause_reason"])
        self.assertEqual(self.host.spawn_calls("design"), [])

    @async_test
    async def test_router_off_keeps_the_original_pause(self) -> None:
        home = TemporaryDirectory()
        self.addCleanup(home.cleanup)
        self._isolate(
            {
                "PA_ROUTER_HOME": home.name,
                "PA_MODEL_ROUTER": "0",
                "PA_ROUTER_PROVIDERS": "antigravity,codex",
                "PA_ROUTER_QUOTA_JSON": self._quota(gemini=0, claude_gpt=0, codex=0),
                "PA_CODEXBAR_BIN": "none",
            }
        )
        self._machine("antigravity/claude-opus-5-5")
        self.host.outcomes["design"] = {"status": "done", "answer": QUOTA_ANSWER}
        started = await self.start()
        await self.executor._runs[started["run_id"]].task
        status = await rlm_module.rlm.factory.status(started["run_id"])

        self.assertEqual(status["pause_reason"], EXACT_PAUSE)
        self.assertEqual(len(self.host.spawn_calls("design")), 1)
        self.assertNotIn("model_rerouted", [event["kind"] for event in status["events"]])

    @async_test
    async def test_unrecognized_models_pause_exactly_as_before(self) -> None:
        home = TemporaryDirectory()
        self.addCleanup(home.cleanup)
        self._isolate(
            {
                "PA_ROUTER_HOME": home.name,
                "PA_MODEL_ROUTER": "1",
                "PA_ROUTER_PROVIDERS": "antigravity,codex,claude,cursor",
                "PA_ROUTER_QUOTA_JSON": self._quota(gemini=0, claude_gpt=0, codex=0),
                "PA_CODEXBAR_BIN": "none",
            }
        )
        self._machine("pi/test-model", prompt="Design.")
        self.host.outcomes["design"] = {"status": "done", "answer": QUOTA_ANSWER}
        started = await self.start()
        await self.executor._runs[started["run_id"]].task
        status = await rlm_module.rlm.factory.status(started["run_id"])

        self.assertEqual(status["pause_reason"], EXACT_PAUSE)
        self.assertEqual(
            [call["kwargs"].get("model") for call in self.host.spawn_calls("design")],
            ["pi/test-model"],
        )


class RampRouterV2Tests(unittest.TestCase):
    def test_switchyard_pair_enforces_2x_output_cost_gate(self) -> None:
        self.assertIsNone(
            model_router.validate_switchyard_pair(
                "antigravity/gemini-3.8-flash",
                "antigravity/gemini-4-neon",
            )
        )
        same_err = model_router.validate_switchyard_pair(
            "antigravity/gemini-4-neon",
            "antigravity/gemini-4-neon",
        )
        self.assertIsNotNone(same_err)
        narrow_err = model_router.validate_switchyard_pair(
            "antigravity/gemini-4-argon",
            "antigravity/gemini-4-neon",
        )
        self.assertIsNotNone(narrow_err)
        self.assertIn("2x", narrow_err or "")

    def test_switchyard_routes_efficient_vs_capable_by_sensitivity(self) -> None:
        preset = model_router.ROUTING_PRESETS["coding-agents-experimental"]
        snap = model_router.QuotaSnapshot.empty(now=1000.0)
        fast_pick = model_router.route_model(
            prompt="Fix typo and format changelog",
            snapshot=snap,
            available={"antigravity", "claude", "codex", "cursor"},
            now=1000.0,
            routing_profile=preset,
        )
        self.assertEqual(fast_pick.model, "antigravity/gemini-3.8-flash")
        self.assertIn("switchyard", fast_pick.reason)

        frontier_pick = model_router.route_model(
            prompt="Security audit and architecture threat model",
            snapshot=snap,
            available={"antigravity", "claude", "codex", "cursor"},
            now=1000.0,
            routing_profile=preset,
        )
        self.assertEqual(frontier_pick.model, "antigravity/gemini-4-neon")
        self.assertIn("capable", frontier_pick.reason)

    def test_throughput_only_priority_prefers_lower_latency_pool(self) -> None:
        snap = model_router.QuotaSnapshot.empty(now=1000.0)
        health = {
            "codex": model_router.ProviderHealth(pool="codex", latency_ewma_ms=9000.0, latency_samples=5),
            "antigravity-gemini": model_router.ProviderHealth(
                pool="antigravity-gemini", latency_ewma_ms=800.0, latency_samples=5
            ),
        }
        decision = model_router.route_model(
            prompt="Fix typo in docs",
            snapshot=snap,
            health=health,
            available={"codex", "antigravity"},
            now=1000.0,
            routing_profile=model_router.ROUTING_PRESETS["low-latency"],
        )
        self.assertEqual(decision.model, "antigravity/gemini-3.8-flash")

    def test_load_routing_profile_reads_ramp_schema_json(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "router-profile.json"
            path.write_text(
                json.dumps(
                    {
                        "name": "My Ramp Profile",
                        "preset_type": "coding-agents-experimental",
                        "provider_routing_preference": "favor_cost",
                        "switchyard_routing_enabled": True,
                        "switchyard_settings": {
                            "efficient_model": "openai-codex/gpt-5.4-mini",
                            "capable_model": "antigravity/gemini-4-neon",
                            "sensitivity_config_name": "frugal",
                        },
                    }
                ),
                encoding="utf-8",
            )
            profile = model_router.load_routing_profile(path)
            self.assertEqual(profile.label, "My Ramp Profile")
            self.assertEqual(profile.provider_priority, "favor_cost")
            self.assertTrue(profile.switchyard.enabled)
            self.assertEqual(profile.switchyard.efficient_model, "openai-codex/gpt-5.4-mini")
            self.assertEqual(profile.switchyard.sensitivity, "frugal")

    def test_detect_ramp_router_gateway_finds_env_or_receipt(self) -> None:
        with TemporaryDirectory() as tmp:
            home = Path(tmp)
            self.assertIsNone(model_router.detect_ramp_router_gateway(env={}, home=home))
            gw_env = model_router.detect_ramp_router_gateway(
                env={"ROUTER_BASE_URL": "https://router.ramp.com", "ROUTER_API_KEY": "rk_test"},
                home=home,
            )
            self.assertEqual(gw_env["source"], "env")
            receipt = home / ".claude" / "ramp-router-state.json"
            receipt.parent.mkdir(parents=True)
            receipt.write_text("{}", encoding="utf-8")
            gw_receipt = model_router.detect_ramp_router_gateway(env={}, home=home)
            self.assertEqual(gw_receipt, {"source": "receipt", "harnesses": ("claude-code",)})

    def test_provider_error_markers_and_cross_process_health_reload(self) -> None:
        for text in (
            "You've hit your limit · resets 2am",
            "Out of extra usage for Claude Max",
            "Usage limit reached for this billing period",
            "HTTP 429 Too Many Requests",
            "Your credit balance is too low to access the Anthropic API.",
        ):
            self.assertTrue(model_router.is_provider_error_text(text), text)
        for benign in ("completed in 429ms", "all tests passed cleanly", ""):
            self.assertFalse(model_router.is_provider_error_text(benign), benign)

        with TemporaryDirectory() as tmp:
            home = Path(tmp)
            with patch.dict(os.environ, {"PA_ROUTER_HOME": str(home), "PA_CODEXBAR_BIN": "none"}, clear=False):
                reset_router()
                router = model_router.get_router()
                self.assertFalse(router.pool_open("claude"))
                # Simulate another process (e.g., Captain) opening the claude circuit on disk.
                other = ModelRouter(home=home)
                other.record_failure("claude", "You've hit your limit")
                reloaded = model_router.get_router()
                self.assertTrue(reloaded.pool_open("claude"))
                reset_router()

    def test_captain_router_pre_routes_and_records_fallback(self) -> None:
        captain_router_path = (
            Path(__file__).resolve().parents[2] / "crates" / "pa-cli" / "src" / "captain_router.py"
        )
        ns: dict[str, object] = {"os": os, "json": json, "Path": Path}
        exec(compile(captain_router_path.read_text(encoding="utf-8"), str(captain_router_path), "exec"), ns)

        quota_exhausted = json.dumps(
            [
                {
                    "provider": "claude",
                    "rateWindowLabels": {"primary": "Session", "secondary": "Weekly"},
                    "usage": {
                        "primary": {"usedPercent": 100, "resetsAt": RESET_SOON},
                        "secondary": {"usedPercent": 100, "resetsAt": RESET_WEEK},
                    },
                },
                {
                    "provider": "antigravity",
                    "extraRateWindows": [
                        {"label": "Gemini weekly", "usedPercent": 15, "resetsAt": RESET_SOON},
                        {"label": "Claude/GPT weekly", "usedPercent": 99, "resetsAt": RESET_WEEK},
                    ],
                },
            ]
        )
        quota_healthy = json.dumps(
            [
                {
                    "provider": "claude",
                    "rateWindowLabels": {"primary": "Session"},
                    "usage": {"primary": {"usedPercent": 20, "resetsAt": RESET_SOON}},
                },
                {
                    "provider": "antigravity",
                    "extraRateWindows": [
                        {"label": "Gemini weekly", "usedPercent": 15, "resetsAt": RESET_SOON},
                        {"label": "Claude/GPT weekly", "usedPercent": 99, "resetsAt": RESET_WEEK},
                    ],
                },
            ]
        )
        with TemporaryDirectory() as tmp:
            env_base = {
                "PA_ROUTER_HOME": tmp,
                "PA_ROUTER_PROVIDERS": "claude,antigravity",
                "PA_CODEXBAR_BIN": "none",
                "PA_MODEL_ROUTER_PY": str(Path(model_router.__file__).resolve()),
            }
            with patch.dict(os.environ, {**env_base, "PA_ROUTER_QUOTA_JSON": quota_exhausted}, clear=False):
                os.environ.pop("PA_MODEL_ROUTER", None)
                ns["_ROUTER_LOOKED"] = False
                ns["_ROUTER_MOD"] = None
                model, thinking, note = ns["captain_route_model"](
                    "anthropic/claude-opus-5-5", "high", prompt="ok go ahead", role="captain", state_id="captain"
                )
                self.assertEqual(model, "antigravity/gemini-4-neon")
                self.assertEqual(thinking, "xhigh")
                self.assertIsNotNone(note)
                self.assertIn("antigravity/gemini-4-neon", note)

            with patch.dict(os.environ, {**env_base, "PA_ROUTER_QUOTA_JSON": quota_healthy}, clear=False):
                os.environ.pop("PA_MODEL_ROUTER", None)
                ns["_ROUTER_LOOKED"] = False
                ns["_ROUTER_MOD"] = None
                model, thinking, note = ns["captain_route_model"](
                    "anthropic/claude-opus-5-5", "high", prompt="ok go ahead", role="captain", state_id="captain"
                )
                self.assertEqual(model, "anthropic/claude-opus-5-5")
                self.assertIsNone(note)

                fb_model, fb_thinking, fb_note, exhausted = ns["captain_record_and_fallback"](
                    "anthropic/claude-opus-5-5",
                    "high",
                    "You've hit your limit · resets 2am",
                    prompt="continue implementation",
                    role="captain",
                    state_id="captain",
                )
                self.assertEqual(fb_model, "antigravity/gemini-4-neon")
                self.assertIsNone(exhausted)
                self.assertIsNotNone(fb_note)
                # Circuit is now persisted on disk so the very next pre-route also avoids claude!
                model2, _, note2 = ns["captain_route_model"](
                    "anthropic/claude-opus-5-5", "high", prompt="next turn", role="captain", state_id="captain"
                )
                self.assertEqual(model2, "antigravity/gemini-4-neon")
                self.assertIsNotNone(note2)

            session_jsonl = Path(tmp) / "captain-session.jsonl"
            session_jsonl.write_text('{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}\n', encoding="utf-8")
            offset = session_jsonl.stat().st_size
            self.assertIsNone(ns["captain_session_quota_error"](str(session_jsonl), offset))
            # A recovered retry episode (details.success == True) must NOT trigger a quota failure.
            with session_jsonl.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps(
                        {
                            "type": "custom_message",
                            "customType": "provider_retry_outcome",
                            "content": "Recovered after 3 retries: You've hit your limit",
                            "display": True,
                            "details": {
                                "success": True,
                                "attempts": 3,
                                "finalError": "You've hit your limit",
                            },
                        }
                    )
                    + "\n"
                )
            self.assertIsNone(ns["captain_session_quota_error"](str(session_jsonl), offset))
            # An unrecovered retry episode (details.success == False) DOES trigger detection.
            with session_jsonl.open("a", encoding="utf-8") as fh:
                fh.write(
                    json.dumps(
                        {
                            "type": "custom_message",
                            "customType": "provider_retry_outcome",
                            "content": "Failed after 3 retries",
                            "display": True,
                            "details": {
                                "success": False,
                                "attempts": 3,
                                "finalError": "You've hit your limit",
                            },
                        }
                    )
                    + "\n"
                )
            found = ns["captain_session_quota_error"](str(session_jsonl), offset)
            self.assertIsNotNone(found)
            self.assertIn("hit your limit", found)
            # Short normal coding answers mentioning 429 / rate limits must not match CLI banners.
            self.assertFalse(
                ns["captain_is_cli_banner_error"](
                    "Added a 429 Too Many Requests response when a client exceeds the usage limit (api/limits.py)."
                )
            )
            self.assertFalse(
                ns["captain_is_cli_banner_error"](
                    "Implemented rate limit reached handling in retry.rs"
                )
            )


if __name__ == "__main__":
    unittest.main()
