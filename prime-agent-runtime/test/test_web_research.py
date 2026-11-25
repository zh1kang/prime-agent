"""Tests for the Exa-inspired web-research standalone machine and composable FSM module."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
MACHINE_DIR = ROOT / "prime-agent-runtime/src/rlm/machines/web-research"
MODULE_DIR = ROOT / "prime-agent-runtime/src/rlm/modules/web-research"
SEARCH_PY = MACHINE_DIR / "nodes/search.py"
RADIATA_BIN = next(
    (p for p in (ROOT / "target/local/radiata", ROOT / "target/debug/radiata") if p.is_file()),
    None,
)


def _compile_dir(path: Path, *params: str) -> dict | None:
    if RADIATA_BIN is None:
        return None
    cmd = [str(RADIATA_BIN), "factory", "compile", str(path)]
    for param in params:
        cmd.extend(["--param", param])
    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise AssertionError(f"radiata factory compile failed ({proc.returncode}):\n{proc.stdout}\n{proc.stderr}")
    return json.loads(proc.stdout)


def _load_search_module():
    spec = importlib.util.spec_from_file_location("web_research_search_mod", SEARCH_PY)
    mod = importlib.util.module_from_spec(spec)
    assert spec and spec.loader
    spec.loader.exec_module(mod)
    return mod


class _FakeResponse:
    def __init__(self, payload: bytes):
        self._payload = payload

    def read(self) -> bytes:
        return self._payload

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False


class WebResearchMachineAndModuleTests(unittest.TestCase):
    def test_standalone_machine_compiles_cleanly(self):
        import tomllib

        manifest = tomllib.loads((MACHINE_DIR / "machine.toml").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "web-research")
        self.assertEqual(
            set(manifest["states"].keys()),
            {
                "decompose",
                "retrieve",
                "investigate-primary",
                "investigate-critical",
                "verify",
                "synthesize",
            },
        )
        compiled = _compile_dir(MACHINE_DIR, "query=test research question")
        if compiled is not None:
            self.assertEqual(compiled.get("errors", []), [])
        # Verify no role ever uses xhigh.
        text = (MACHINE_DIR / "machine.toml").read_text(encoding="utf-8")
        self.assertNotIn("xhigh", text)
        self.assertIn('model = "router"', text)

    def test_composable_module_vendors_and_compiles_inside_project_fsm(self):
        import tomllib

        module_manifest = tomllib.loads((MODULE_DIR / "module.toml").read_text(encoding="utf-8"))
        self.assertEqual(module_manifest["name"], "web-research")
        self.assertEqual(module_manifest["entry"], ["decompose"])
        self.assertEqual(module_manifest["exit"], "synthesize")
        self.assertNotIn("xhigh", (MODULE_DIR / "module.toml").read_text(encoding="utf-8"))

        # Load captain_modules helper and actually vendor web-research into a minimal project FSM.
        mod_code = (ROOT / "crates/radiata-cli/src/captain_modules.py").read_text(encoding="utf-8")
        ns: dict = {
            "__builtins__": __builtins__,
            "contextmanager": contextlib.contextmanager,
            "FACTORY_NAME": __import__("re").compile(r"[a-z0-9][a-z0-9-]{0,63}"),
            "fsync_dir": lambda _p: None,
            "tempfile": tempfile,
            "json": json,
            "os": os,
            "re": __import__("re"),
            "shutil": __import__("shutil"),
            "hashlib": __import__("hashlib"),
            "tomllib": tomllib,
            "Path": Path,
        }
        def _atomic_write(path, content):
            Path(path).write_text(content, encoding="utf-8")

        def _snapshot_files(src, dst):
            __import__("shutil").copytree(src, dst, dirs_exist_ok=True)

        ns["atomic_write"] = _atomic_write
        ns["snapshot_files"] = _snapshot_files
        exec(compile(mod_code, "captain_modules.py", "exec"), ns)

        with tempfile.TemporaryDirectory() as tmp:
            fsm_dir = Path(tmp) / "demo-fsm"
            (fsm_dir / "prompts").mkdir(parents=True)
            (fsm_dir / "prompts/seed.md").write_text("Produce topic: {{task}}\n", encoding="utf-8")
            (fsm_dir / "prompts/plan.md").write_text("Plan from research:\n{{research}}\n", encoding="utf-8")
            (fsm_dir / "machine.toml").write_text(
                'format = 1\nname = "demo-fsm"\nversion = 1\ndescription = "demo"\n\n'
                '[params]\ntask = { type = "text", description = "task" }\n\n'
                '[roles]\ndefault = { model = "router", thinking = "high" }\n\n'
                '[states.seed]\nentry = true\nagent = { role = "default", prompt = "prompts/seed.md" }\n'
                'outputs = { topic = "text" }\n\n'
                '[states.plan]\nagent = { role = "default", prompt = "prompts/plan.md" }\n'
                'inputs = { research = "web-research-synthesize.report" }\n'
                'outputs = { plan = "text" }\n',
                encoding="utf-8",
            )
            mod_record = {
                "name": module_manifest["name"],
                "version": module_manifest["version"],
                "entry": module_manifest["entry"],
                "exit": module_manifest["exit"],
                "wire": module_manifest["wire"],
                "manifest": module_manifest,
                "dir": str(MODULE_DIR),
                "source": "builtin",
                "digest": "test-digest",
            }
            ns["vendor_module"](
                fsm_dir,
                mod_record,
                "web-research",
                {"subject": "seed.topic", "done": "plan"},
                "seed",
            )
            compiled = _compile_dir(fsm_dir, "task=test research topic")
            if compiled is not None:
                self.assertEqual(compiled.get("errors", []), [])

    def test_retrieve_uses_exa_highlights_dedups_and_filters_prior_urls(self):
        mod = _load_search_module()
        captured_requests = []

        def fake_urlopen(req, timeout=15.0):
            captured_requests.append((req.full_url, json.loads(req.data.decode("utf-8"))))
            payload = {
                "results": [
                    {
                        "title": "Exa Neural Search Paper",
                        "url": "https://arxiv.org/abs/2501.00001/?utm_source=twitter",
                        "publishedDate": "2025-01-15T00:00:00Z",
                        "highlights": ["Neural embeddings retrieve relevant passages directly."],
                    },
                    {
                        "title": "Duplicate Same URL",
                        "url": "https://arxiv.org/abs/2501.00001",
                        "publishedDate": "2025-01-15",
                        "highlights": ["Should be deduplicated."],
                    },
                    {
                        "title": "Already Seen Source",
                        "url": "https://example.com/old",
                        "publishedDate": "2024-05-01",
                        "highlights": ["Filtered out by prior_urls."],
                    },
                ]
            }
            return _FakeResponse(json.dumps(payload).encode("utf-8"))

        with mock.patch.dict(os.environ, {"EXA_API_KEY": "test-exa-key"}, clear=False):
            with mock.patch.object(mod.urllib.request, "urlopen", side_effect=fake_urlopen):
                out = mod.retrieve(
                    {
                        "queries": ["neural search architecture site:arxiv.org"],
                        "prior_urls": ["https://www.example.com/old/"],
                    }
                )

        self.assertEqual(out["backend"], "exa")
        self.assertEqual(len(captured_requests), 1)
        url, body = captured_requests[0]
        self.assertEqual(url, "https://api.exa.ai/search")
        self.assertEqual(body["includeDomains"], ["arxiv.org"])
        self.assertIn("highlights", body["contents"])
        self.assertIn("Exa Neural Search Paper", out["evidence"])
        self.assertNotIn("Should be deduplicated", out["evidence"])
        self.assertNotIn("Filtered out by prior_urls", out["evidence"])
        self.assertEqual(len(out["urls"]), 2)  # prior + 1 new unique

    def test_retrieve_falls_back_cleanly_to_open_apis_without_exa_key(self):
        mod = _load_search_module()
        arxiv_xml = b"""<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <entry>
            <id>https://arxiv.org/abs/2401.12345</id>
            <title>Continual Plasticity Benchmark</title>
            <published>2024-01-20T00:00:00Z</published>
            <summary>Measures loss of plasticity across continual streams.</summary>
          </entry>
        </feed>"""

        def fake_urlopen(req, timeout=15.0):
            if "arxiv.org" in req.full_url:
                return _FakeResponse(arxiv_xml)
            return _FakeResponse(b'{"items": []}')

        env = {k: v for k, v in os.environ.items() if k != "EXA_API_KEY"}
        with mock.patch.dict(os.environ, env, clear=True):
            with mock.patch.object(mod.urllib.request, "urlopen", side_effect=fake_urlopen):
                out = mod.retrieve({"queries": ["plasticity loss benchmark paper"]})

        self.assertEqual(out["backend"], "open_api_fallback")
        self.assertIn("Continual Plasticity Benchmark", out["evidence"])
        self.assertIn("https://arxiv.org/abs/2401.12345", out["urls"])


if __name__ == "__main__":
    unittest.main()
