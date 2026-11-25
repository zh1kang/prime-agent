"""Behavioral tests for Captain crew delegation, Capy-style subagent prompt, and model='router'."""

from __future__ import annotations

import importlib.util
import sys
import types
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
ROUTER_PY = ROOT / "crates/radiata-cli/src/captain_router.py"
JOBS_PY = ROOT / "crates/radiata-cli/src/captain_jobs.py"


def _load_captain_router():
    spec = importlib.util.spec_from_file_location("captain_router_test_mod", ROUTER_PY)
    mod = importlib.util.module_from_spec(spec)
    assert spec and spec.loader
    spec.loader.exec_module(mod)
    return mod


class CaptainDelegateAndRouterTests(unittest.TestCase):
    def test_captain_route_model_handles_router_sentinel_dynamically(self):
        mod = _load_captain_router()
        fake_decision = types.SimpleNamespace(
            model="anthropic/claude-opus-5-5",
            thinking="high",
            reason="selected for complex task",
            exhausted=False,
        )
        fake_router = types.SimpleNamespace(
            routed_step_decision=mock.Mock(return_value=fake_decision),
            admission_decision=mock.Mock(side_effect=AssertionError("should not call admission_decision for router")),
        )
        with mock.patch.object(mod, "_router_module", return_value=fake_router):
            chosen, thinking, note = mod.captain_route_model(
                "router",
                "",
                prompt="audit the training script",
                role="reviewer",
                state_id="delegate",
            )
        self.assertEqual(chosen, "anthropic/claude-opus-5-5")
        self.assertEqual(thinking, "high")
        self.assertIn("router -> anthropic/claude-opus-5-5", note or "")
        fake_router.routed_step_decision.assert_called_once()

    def test_captain_route_model_router_returns_empty_when_disabled_or_exhausted(self):
        mod = _load_captain_router()
        fake_router = types.SimpleNamespace(routed_step_decision=mock.Mock(return_value=None))
        with mock.patch.object(mod, "_router_module", return_value=fake_router):
            chosen, thinking, note = mod.captain_route_model("router", "medium", prompt="hi")
        self.assertEqual(chosen, "")
        self.assertEqual(thinking, "medium")
        self.assertIsNone(note)

    def test_leader_prompt_has_capy_crew_subagent_rule_and_shell_instructions(self):
        text = JOBS_PY.read_text(encoding="utf-8")
        self.assertIn("via your shell tool (Bash, run_command, or similar)", text)
        self.assertIn("never spawn an ephemeral internal subagent", text)
        self.assertIn("delegate(provider,task,write,request_id,title?,model?,thinking?,specialist?)", text)
        self.assertIn("model == ROUTER_MODEL", text)
        self.assertIn("captain_route_model(\n                        ROUTER_MODEL", text)


if __name__ == "__main__":
    unittest.main()
