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
            self.assertTrue(
                {"period", "used_percent", "resets_at"}.issubset(bucket), bucket
            )
            self.assertTrue(
                set(bucket).issubset(
                    {"period", "scope", "label", "window", "used_percent", "resets_at"}
                ),
                bucket,
            )
            self.assertIn(bucket["period"], {"session", "weekly", "monthly", "other"})
            if "scope" in bucket:
                self.assertIn(bucket["scope"], {"overall", "model"})
            if "label" in bucket:
                self.assertIsInstance(bucket["label"], str)
            if "window" in bucket:
                self.assertIsInstance(bucket["window"], str)
            self.assertIsInstance(bucket["used_percent"], int)
            self.assertTrue(bucket["resets_at"] is None or isinstance(bucket["resets_at"], int))

    def test_openai_normalization_is_sanitized(self) -> None:
        result, value = self.normalize_cli("openai", "openai-rate-limits.json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_contract(value, "openai")
        self.assertEqual(
            value["buckets"],
            [
                {
                    "period": "session",
                    "used_percent": 7,
                    "resets_at": 1893474000,
                    "label": "Private Model Name",
                    "window": "primary",
                },
                {
                    "period": "weekly",
                    "scope": "overall",
                    "used_percent": 30,
                    "resets_at": 1893456000,
                    "label": "Codex",
                    "window": "primary",
                },
            ],
        )
        self.assertIn(b"Private Model Name", result.stdout)
        self.assertNotIn(b"private-model-id", result.stdout)
        self.assertNotIn(b"private-plan", result.stdout)
        self.assertNotIn(b"availableCount", result.stdout)

    def test_openai_drops_unsafe_labels_without_leaking_limit_ids(self) -> None:
        raw = json.loads((FIXTURES / "openai-rate-limits.json").read_text(encoding="utf-8"))
        private = raw["result"]["rateLimitsByLimitId"]["private-model-id"]
        private["limitName"] = "unsafe\nlabel"
        value = usage_probe.normalize_openai(raw, 1893450000)
        bucket = next(bucket for bucket in value["buckets"] if bucket["period"] == "session")
        self.assertNotIn("label", bucket)
        self.assertNotIn("scope", bucket)
        self.assertNotIn("private-model-id", json.dumps(value))

        private["limitName"] = "x" * 129
        value = usage_probe.normalize_openai(raw, 1893450000)
        bucket = next(bucket for bucket in value["buckets"] if bucket["period"] == "session")
        self.assertNotIn("label", bucket)

    def test_legacy_protocol_remains_compatible_with_strict_old_hosts(self) -> None:
        raw = json.loads((FIXTURES / "openai-rate-limits.json").read_text(encoding="utf-8"))
        value = usage_probe._legacy_protocol(usage_probe.normalize_openai(raw, 1893450000))
        for bucket in value["buckets"]:
            self.assertEqual(set(bucket), {"period", "scope", "used_percent", "resets_at"})
            self.assertIn(bucket["scope"], {"overall", "model"})
        self.assertNotIn("private-model-id", json.dumps(value))

    def test_openai_normalizes_millisecond_reset_epochs(self) -> None:
        raw = json.loads((FIXTURES / "openai-rate-limits.json").read_text(encoding="utf-8"))
        window = raw["result"]["rateLimitsByLimitId"]["codex"]["primary"]
        window["resetsAt"] = 1893456000123
        value = usage_probe.normalize_openai(raw, 1893450000)
        self.assertEqual(value["buckets"][-1]["resets_at"], 1893456000)

        window["resetsAt"] = 10**18
        value = usage_probe.normalize_openai(raw, 1893450000)
        self.assertIsNone(value["buckets"][-1]["resets_at"])

    def test_ollama_normalization_is_sanitized(self) -> None:
        result, value = self.normalize_cli("ollama", "ollama-usage.json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_contract(value, "ollama")
        self.assertEqual([bucket["used_percent"] for bucket in value["buckets"]], [12, 3])
        self.assertEqual([bucket["label"] for bucket in value["buckets"]], ["session", "weekly"])
        self.assertTrue(all(bucket["resets_at"] is None for bucket in value["buckets"]))
        self.assertNotIn(b"cost", result.stdout)

    def test_anthropic_normalization_uses_snapshot_time_and_preserves_model_name(self) -> None:
        result, value = self.normalize_cli("anthropic", "anthropic-usage.json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_contract(value, "anthropic")
        self.assertEqual(value["observed_at"], 1893456000)
        self.assertEqual([bucket["used_percent"] for bucket in value["buckets"]], [16, 3, 4])
        self.assertEqual(value["buckets"][-1]["label"], "Private Anthropic Model")
        self.assertEqual(value["buckets"][-1]["scope"], "model")
        self.assertIn(b"Private Anthropic Model", result.stdout)
        self.assertNotIn(b"extra_usage", result.stdout)
        self.assertNotIn(b"monthly_limit", result.stdout)

    def test_anthropic_model_scope_does_not_depend_on_label_safety(self) -> None:
        source = json.loads((FIXTURES / "anthropic-usage.json").read_text(encoding="utf-8"))
        limit = source["cachedUsageUtilization"]["utilization"]["limits"][-1]
        limit["kind"] = "provider_specific"
        for unsafe in ("unsafe\nlabel", "\N{NO-BREAK SPACE}", "x" * 129):
            limit["scope"]["model"]["display_name"] = unsafe
            value = usage_probe.normalize_anthropic(source)
            bucket = next(
                bucket for bucket in value["buckets"] if bucket.get("window") == "provider_specific"
            )
            self.assertEqual(bucket["scope"], "model")
            self.assertNotIn("label", bucket)

        del limit["scope"]
        value = usage_probe.normalize_anthropic(source)
        bucket = next(
            bucket for bucket in value["buckets"] if bucket.get("window") == "provider_specific"
        )
        self.assertNotIn("scope", bucket)

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
        self.assertIn("Private Model Name", encoded)
        self.assertNotIn("private-model-id", encoded)
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
            with mock.patch.dict(
                os.environ,
                {
                    "OLLAMA_API_KEY": "test-only-key",
                    "HTTP_PROXY": "http://127.0.0.1:1",
                    "http_proxy": "http://127.0.0.1:1",
                    "NO_PROXY": "127.0.0.1",
                    "no_proxy": "127.0.0.1",
                },
            ):
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
                "'early':early,'columns':size.columns,'lines':size.lines,"
                "'path':os.environ.get('PATH'),'http_proxy':os.environ.get('HTTP_PROXY'),"
                "'unrelated':os.environ.get('PROBE_UNRELATED_SECRET'),"
                "'anthropic_base':os.environ.get('ANTHROPIC_BASE_URL')}\n"
                "pathlib.Path(sys.argv[0]).with_name('record.json').write_text(json.dumps(record))\n"
                f"state.write_text({fixture!r})\n"
                "print('usage opened',flush=True)\n"
                "sys.stdin.readline()\n",
            )
            with mock.patch.dict(
                os.environ,
                {
                    "PROBE_UNRELATED_SECRET": "must-not-pass",
                    "ANTHROPIC_BASE_URL": "https://must-not-pass.invalid",
                    "HTTP_PROXY": "http://proxy.example.test:8080",
                },
            ):
                value = usage_probe.probe_anthropic(
                    claude_bin=str(script), secure_storage=secure, timeout=5
                )
            invocation = json.loads(record.read_text(encoding="utf-8"))
        self.assertEqual(value["provider"], "anthropic")
        self.assertEqual(invocation["line"], "/usage")
        self.assertTrue(invocation["trusted"])
        self.assertEqual(invocation["secure"], str(secure))
        self.assertTrue(invocation["path"])
        self.assertEqual(invocation["http_proxy"], "http://proxy.example.test:8080")
        self.assertIsNone(invocation["unrelated"])
        self.assertIsNone(invocation["anthropic_base"])
        self.assertNotEqual(invocation["home"], str(Path.home()))
        self.assertNotEqual(invocation["home"], invocation["state"])
        self.assertFalse(invocation["early"])
        self.assertEqual((invocation["columns"], invocation["lines"]), (120, 40))
        self.assertIn("--safe-mode", invocation["argv"])
        self.assertNotIn("--ax-screen-reader", invocation["argv"])
        self.assertIn("--no-chrome", invocation["argv"])
        self.assertEqual(invocation["argv"][invocation["argv"].index("--permission-mode") + 1], "plan")
        self.assertEqual(invocation["argv"][invocation["argv"].index("--tools") + 1], "")

    def test_nonblocking_write_handles_blocking_and_partial_progress_once(self) -> None:
        read_fd, write_fd = os.pipe()
        os.set_blocking(write_fd, False)
        real_write = os.write
        calls: list[bytes] = []

        def flaky_write(fd: int, data: bytes | memoryview) -> int:
            chunk = bytes(data)
            calls.append(chunk)
            if len(calls) == 1:
                raise BlockingIOError()
            if len(calls) == 2:
                return real_write(fd, chunk[:3])
            return real_write(fd, chunk)

        try:
            with mock.patch.object(usage_probe.os, "write", side_effect=flaky_write):
                usage_probe._write_nonblocking(write_fd, b"/usage\r", usage_probe.time.monotonic() + 1)
            self.assertEqual(os.read(read_fd, 64), b"/usage\r")
        finally:
            os.close(read_fd)
            os.close(write_fd)
        self.assertEqual(calls, [b"/usage\r", b"/usage\r", b"age\r"])

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
