"""The validate module's check node: flaky re-runs and failure-log trimming."""

from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

NODE = Path(__file__).resolve().parents[1] / "src/rlm/modules/validate/nodes/validate.py"
_spec = importlib.util.spec_from_file_location("validate_node", NODE)
validate = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(validate)


class ValidateNodeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.marker = Path(self.dir.name) / "ran"
        env = {key: value for key, value in os.environ.items() if key != "PA_VALIDATE_FLAKY_RETRIES"}
        self.env = patch.dict(os.environ, env, clear=True)
        self.env.start()

    def tearDown(self) -> None:
        self.env.stop()
        self.dir.cleanup()

    def fails_once(self) -> str:
        return f"if [ -e {self.marker} ]; then echo ok; else touch {self.marker}; echo boom; exit 1; fi"

    def test_requires_a_command(self) -> None:
        with self.assertRaises(ValueError):
            validate.run({})

    def test_passing_and_failing_keep_the_output_contract(self) -> None:
        self.assertEqual(validate.run({"validate_command": "echo hi"}), {"passed": True, "log": "$ echo hi\nexit 0\nhi\n"})
        failed = validate.run({"validate_command": "echo no; exit 3"})
        self.assertEqual(set(failed), {"passed", "log"})
        self.assertFalse(failed["passed"])
        self.assertIn("exit 3", failed["log"])

    def test_a_failure_is_final_without_flaky_retries(self) -> None:
        result = validate.run({"validate_command": self.fails_once()})
        self.assertFalse(result["passed"])

    def test_a_check_that_passes_on_rerun_is_marked_flaky(self) -> None:
        result = validate.run({"validate_command": self.fails_once(), "flaky_retries": 1})
        self.assertEqual(set(result), {"passed", "log"})
        self.assertTrue(result["passed"])
        self.assertIn("flaky: exit 1 first, then passed on re-run 1", result["log"])

    def test_the_env_var_enables_flaky_retries(self) -> None:
        os.environ["PA_VALIDATE_FLAKY_RETRIES"] = "1"
        self.assertTrue(validate.run({"validate_command": self.fails_once()})["passed"])

    def test_rerun_failed_cmd_reruns_with_its_own_command(self) -> None:
        result = validate.run({"validate_command": "exit 1", "rerun_failed_cmd": "echo rerun-ok"})
        self.assertTrue(result["passed"])
        self.assertIn("rerun-ok", result["log"])

    def test_a_consistent_failure_still_fails_after_reruns(self) -> None:
        result = validate.run({"validate_command": "echo nope; exit 2", "flaky_retries": 2})
        self.assertFalse(result["passed"])
        self.assertIn("exit 2", result["log"])

    def test_failure_log_keeps_the_tail_and_earlier_error_lines(self) -> None:
        command = "echo 'error: first cause'; for i in $(seq 1 500); do echo line $i; done; exit 1"
        log = validate.run({"validate_command": command})["log"]
        self.assertIn("error: first cause", log)
        self.assertIn("[last 80 of 501 lines]", log)
        self.assertIn("line 500", log)
        self.assertNotIn("line 100\n", log)


if __name__ == "__main__":
    unittest.main()
