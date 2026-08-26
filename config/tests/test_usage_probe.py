#!/usr/bin/env python3

from __future__ import annotations

import http.server
import importlib.util
import json
import os
import stat
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock


CONFIG_DIR = Path(__file__).resolve().parents[1]
PROBE_PATH = CONFIG_DIR / "usage-probe.py"
FIXTURES = Path(__file__).resolve().parent / "fixtures"
SPEC = importlib.util.spec_from_file_location("usage_probe", PROBE_PATH)
assert SPEC and SPEC.loader
usage_probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(usage_probe)


class NormalizeTests(unittest.TestCase):
    def normalize_cli(self, provider: str, fixture: str) -> tuple[subprocess.CompletedProcess[bytes], dict]:
        source = (FIXTURES / fixture).read_bytes()
        result = subprocess.run(
            [str(PROBE_PATH), "--normalize", provider],
            input=source,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=3,
        )
        value = json.loads(result.stdout) if result.stdout else {}
        return result, value

    def assert_contract(self, value: dict, provider: str) -> None:
        self.assertEqual(set(value), {"schema_version", "provider", "observed_at", "buckets"})
        self.assertEqual(value["schema_version"], 1)
        self.assertEqual(value["provider"], provider)
        self.assertIsInstance(value["observed_at"], int)
        self.assertGreater(len(value["buckets"]), 0)
        for bucket in value["buckets"]:
            self.assertEqual(set(bucket), {"period", "scope", "used_percent", "resets_at"})
            self.assertIn(bucket["period"], {"session", "weekly", "monthly", "other"})
            self.assertIn(bucket["scope"], {"overall", "model"})
            self.assertIsInstance(bucket["used_percent"], int)
            self.assertTrue(bucket["resets_at"] is None or isinstance(bucket["resets_at"], int))

    def test_openai_normalization_is_sanitized(self) -> None:
        result, value = self.normalize_cli("openai", "openai-rate-limits.json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_contract(value, "openai")
        self.assertEqual(
            value["buckets"],
            [
                {"period": "session", "scope": "model", "used_percent": 7, "resets_at": 1893474000},
                {"period": "weekly", "scope": "overall", "used_percent": 30, "resets_at": 1893456000},
            ],
        )
        self.assertNotIn(b"Private", result.stdout)
        self.assertNotIn(b"private-plan", result.stdout)
        self.assertNotIn(b"availableCount", result.stdout)

    def test_ollama_normalization_is_sanitized(self) -> None:
        result, value = self.normalize_cli("ollama", "ollama-usage.json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_contract(value, "ollama")
        self.assertEqual([bucket["used_percent"] for bucket in value["buckets"]], [12, 3])
        self.assertTrue(all(bucket["resets_at"] is None for bucket in value["buckets"]))
        self.assertNotIn(b"cost", result.stdout)

    def test_anthropic_normalization_uses_snapshot_time_and_hides_names(self) -> None:
        result, value = self.normalize_cli("anthropic", "anthropic-usage.json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_contract(value, "anthropic")
        self.assertEqual(value["observed_at"], 1893456000)
        self.assertEqual([bucket["used_percent"] for bucket in value["buckets"]], [16, 3, 4])
        self.assertNotIn(b"Private", result.stdout)
        self.assertNotIn(b"extra_usage", result.stdout)
        self.assertNotIn(b"monthly_limit", result.stdout)

    def test_empty_or_malformed_input_fails_without_stdout(self) -> None:
        for provider, raw in (("ollama", b'{"limits":{}}'), ("openai", b"not-json")):
            result = subprocess.run(
                [str(PROBE_PATH), "--normalize", provider],
                input=raw,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=3,
            )
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout, b"")
            self.assertRegex(result.stderr, rb"^usage-probe:[a-z-]+\n$")
            self.assertNotIn(raw, result.stderr)


class AcquisitionTests(unittest.TestCase):
    def executable(self, directory: Path, name: str, source: str) -> Path:
        target = directory / name
        target.write_text(source, encoding="utf-8")
        target.chmod(target.stat().st_mode | stat.S_IXUSR)
        return target

    def test_openai_json_rpc_uses_account_read_and_sanitizes(self) -> None:
        fixture = (FIXTURES / "openai-rate-limits.json").read_text(encoding="utf-8")
        with tempfile.TemporaryDirectory() as temporary:
            script = self.executable(
                Path(temporary),
                "fake-codex",
                "#!/usr/bin/env python3\n"
                "import json,sys\n"
                f"result=json.loads({fixture!r})\n"
                "for line in sys.stdin:\n"
                " request=json.loads(line)\n"
                " if request.get('id') == 1:\n"
                "  print(json.dumps({'jsonrpc':'2.0','id':1,'result':{}}),flush=True)\n"
                " elif request.get('id') == 2:\n"
                "  print(json.dumps(result),flush=True)\n",
            )
            value = usage_probe.probe_openai(codex_bin=str(script), timeout=2)
        self.assertEqual(value["provider"], "openai")
        encoded = json.dumps(value)
        self.assertNotIn("Private", encoded)
        self.assertNotIn("private-plan", encoded)

    def test_ollama_calls_only_usage_endpoint(self) -> None:
        fixture = (FIXTURES / "ollama-usage.json").read_bytes()
        requests: list[tuple[str, str | None]] = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                requests.append((self.path, self.headers.get("Authorization")))
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(fixture)))
                self.end_headers()
                self.wfile.write(fixture)

            def do_POST(self) -> None:
                self.send_response(500)
                self.end_headers()

            def log_message(self, _format: str, *args: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with mock.patch.dict(os.environ, {"OLLAMA_API_KEY": "test-only-key"}):
                value = usage_probe.probe_ollama(
                    url=f"http://127.0.0.1:{server.server_port}/api/usage", timeout=2
                )
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)
        self.assertEqual(value["provider"], "ollama")
        self.assertEqual(requests, [("/api/usage", "Bearer test-only-key")])

    def test_anthropic_uses_isolated_safe_pty_and_only_usage_command(self) -> None:
        fixture = (FIXTURES / "anthropic-usage.json").read_text(encoding="utf-8")
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            record = directory / "record.json"
            secure = directory / "secure"
            secure.mkdir()
            script = self.executable(
                directory,
                "fake-claude",
                "#!/usr/bin/env python3\n"
                "import json,os,pathlib,select,sys\n"
                "state=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])/'.claude.json'\n"
                "before=json.loads(state.read_text())\n"
                "early=bool(select.select([sys.stdin],[],[],2.2)[0])\n"
                "print('ready ❯',flush=True)\n"
                "line=sys.stdin.readline().strip()\n"
                "size=os.get_terminal_size(0)\n"
                "record={'argv':sys.argv[1:],'line':line,'cwd':os.getcwd(),"
                "'home':os.environ['HOME'],'state':os.environ['CLAUDE_CONFIG_DIR'],"
                "'secure':os.environ['CLAUDE_SECURESTORAGE_CONFIG_DIR'],"
                "'trusted':before['projects'][os.getcwd()]['hasTrustDialogAccepted'],"
                "'early':early,'columns':size.columns,'lines':size.lines}\n"
                "pathlib.Path(os.environ['FAKE_CLAUDE_RECORD']).write_text(json.dumps(record))\n"
                f"state.write_text({fixture!r})\n"
                "print('usage opened',flush=True)\n"
                "sys.stdin.readline()\n",
            )
            with mock.patch.dict(os.environ, {"FAKE_CLAUDE_RECORD": str(record)}):
                value = usage_probe.probe_anthropic(
                    claude_bin=str(script), secure_storage=secure, timeout=5
                )
            invocation = json.loads(record.read_text(encoding="utf-8"))
        self.assertEqual(value["provider"], "anthropic")
        self.assertEqual(invocation["line"], "/usage")
        self.assertTrue(invocation["trusted"])
        self.assertEqual(invocation["secure"], str(secure))
        self.assertNotEqual(invocation["home"], str(Path.home()))
        self.assertNotEqual(invocation["home"], invocation["state"])
        self.assertFalse(invocation["early"])
        self.assertEqual((invocation["columns"], invocation["lines"]), (120, 40))
        self.assertIn("--safe-mode", invocation["argv"])
        self.assertNotIn("--ax-screen-reader", invocation["argv"])
        self.assertIn("--no-chrome", invocation["argv"])
        self.assertEqual(invocation["argv"][invocation["argv"].index("--permission-mode") + 1], "plan")
        self.assertEqual(invocation["argv"][invocation["argv"].index("--tools") + 1], "")

    def test_anthropic_timeout_is_bounded_and_sanitized(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            secure = directory / "secure"
            secure.mkdir()
            script = self.executable(
                directory,
                "hanging-claude",
                "#!/usr/bin/env python3\nimport time\nprint('ready',flush=True)\ntime.sleep(30)\n",
            )
            started = usage_probe.time.monotonic()
            with self.assertRaises(usage_probe.ProbeError) as failure:
                usage_probe.probe_anthropic(
                    claude_bin=str(script), secure_storage=secure, timeout=0.5
                )
            elapsed = usage_probe.time.monotonic() - started
        self.assertEqual(failure.exception.reason, "timeout")
        self.assertLess(elapsed, 2.0)


if __name__ == "__main__":
    unittest.main()
