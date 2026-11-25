"""review-triad verdict merge and RLMChildResult usage extraction."""

import importlib.util
import sys
import unittest
from pathlib import Path

SRC = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SRC))

from rlm import _child_result_from_payload  # noqa: E402

_spec = importlib.util.spec_from_file_location(
    "review_triad_verdict", SRC / "rlm/modules/review-triad/nodes/verdict.py"
)
verdict = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(verdict)


def _payload(**extra):
    return {"rlm_child_id": "c1", "status": "done", "settled": True, **extra}


class ChildUsageTest(unittest.TestCase):
    def test_defaults_without_usage(self):
        result = _child_result_from_payload(_payload())
        self.assertEqual((result.total_tokens, result.cost_usd), (0, 0.0))

    def test_snake_case_usage(self):
        result = _child_result_from_payload(_payload(usage={"total_tokens": 1200, "cost_usd": 0.25}))
        self.assertEqual((result.total_tokens, result.cost_usd), (1200, 0.25))

    def test_camel_case_and_nested_cost(self):
        result = _child_result_from_payload(_payload(usage={"totalTokens": 7, "cost": {"total": 1.5}}))
        self.assertEqual((result.total_tokens, result.cost_usd), (7, 1.5))
        result = _child_result_from_payload(_payload(usage={"costUsd": 2}))
        self.assertEqual(result.cost_usd, 2.0)

    def test_top_level_fields(self):
        result = _child_result_from_payload(_payload(total_tokens=9, cost_usd=0.01))
        self.assertEqual((result.total_tokens, result.cost_usd), (9, 0.01))


class VerdictMergeTest(unittest.TestCase):
    def test_all_approve(self):
        out = verdict.merge({"correctness": True, "bugs": True, "robustness": True,
                             "bugs_findings": "- nit: fine"})
        self.assertEqual(out, {"verdict": "approve", "findings": ""})

    def test_dedup_across_reviewers(self):
        same = "- High: [bug] `src/a.rs:12` Off-by-one in loop bound."
        out = verdict.merge({
            "correctness": False, "correctness_findings": same,
            "bugs": False, "bugs_findings": same.replace("Off-by-one", "off-by-one") + "\n- low: style nit",
            "robustness": True, "robustness_findings": "- ignored because approved",
        })
        self.assertEqual(out["verdict"], "rework")
        lines = out["findings"].splitlines()
        self.assertEqual(len(lines), 2)
        self.assertIn("high bug src/a.rs:12", lines[0])
        self.assertIn("(correctness, bugs)", lines[0])
        self.assertTrue(lines[1].endswith("(bugs)"))
        self.assertNotIn("ignored", out["findings"])

    def test_stable_ids(self):
        inputs = {"correctness": False, "correctness_findings": "x.py:3 crash on empty input"}
        self.assertEqual(verdict.merge(inputs), verdict.merge(dict(inputs)))

    def test_low_and_nit_only_findings_converge_to_approve(self):
        out = verdict.merge({
            "correctness": True, "correctness_findings": "",
            "bugs": False, "bugs_findings": "- low: `src/a.rs:10` rename local variable\n- nit: trailing comment",
            "robustness": False, "robustness_findings": "- minor: `src/b.rs:4` could tighten docstring",
        })
        self.assertEqual(out["verdict"], "approve")
        self.assertEqual(len(out["findings"].splitlines()), 3)

    def test_unexplained_rejection_still_requires_rework(self):
        out = verdict.merge({
            "correctness": False, "correctness_findings": "",
            "bugs": True, "bugs_findings": "",
            "robustness": True, "robustness_findings": "",
        })
        self.assertEqual(out["verdict"], "rework")


if __name__ == "__main__":
    unittest.main()
