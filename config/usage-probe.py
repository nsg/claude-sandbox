#!/usr/bin/env python3
"""Collect and sanitize subscription usage inside the managed container."""

from __future__ import annotations

import datetime as dt
import fcntl
import json
import math
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


SCHEMA_VERSION = 1
MAX_RAW_BYTES = 256 * 1024
MAX_TUI_BYTES = 256 * 1024
DEFAULT_TIMEOUT = 20.0
TUI_ROWS = 40
TUI_COLUMNS = 120
OLLAMA_USAGE_URL = "https://ollama.com/api/usage"
PROVIDERS = {"anthropic", "openai", "ollama"}


class ProbeError(Exception):
    """A failure safe to classify without exposing provider output."""

    def __init__(self, reason: str):
        super().__init__(reason)
        self.reason = reason


def _fail(reason: str) -> ProbeError:
    return ProbeError(reason)


def _object(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise _fail("invalid-response")
    return value


def _array(value: Any) -> list[Any]:
    if not isinstance(value, list):
        raise _fail("invalid-response")
    return value


def _number(value: Any) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise _fail("invalid-response")
    number = float(value)
    if not math.isfinite(number):
        raise _fail("invalid-response")
    return number


def _percent(value: Any, *, fraction: bool = False) -> int:
    number = _number(value)
    if fraction:
        number *= 100
    return math.floor(min(100.0, max(0.0, number)))


def _epoch(value: Any) -> int | None:
    if value is None:
        return None
    number = _number(value)
    if number < 0 or not number.is_integer():
        raise _fail("invalid-response")
    return int(number)


def _rfc3339_epoch(value: Any) -> int | None:
    if value is None:
        return None
    if not isinstance(value, str) or len(value) > 64:
        raise _fail("invalid-response")
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as error:
        raise _fail("invalid-response") from error
    if parsed.tzinfo is None:
        raise _fail("invalid-response")
    return int(parsed.timestamp())


def _period_from_minutes(value: Any) -> str:
    minutes = _number(value)
    if minutes <= 0:
        raise _fail("invalid-response")
    if minutes <= 1_440:
        return "session"
    if minutes <= 20_160:
        return "weekly"
    return "monthly"


def _period_from_name(value: Any) -> str:
    if not isinstance(value, str):
        return "other"
    name = value.lower()
    return name if name in {"session", "weekly", "monthly", "other"} else "other"


def _bucket(period: str, scope: str, used_percent: int, resets_at: int | None) -> dict[str, Any]:
    return {
        "period": period,
        "scope": scope,
        "used_percent": used_percent,
        "resets_at": resets_at,
    }


def _protocol(provider: str, observed_at: int, buckets: list[dict[str, Any]]) -> dict[str, Any]:
    if provider not in PROVIDERS or observed_at < 0 or not buckets or len(buckets) > 32:
        raise _fail("invalid-response")
    unique = {
        (bucket["period"], bucket["scope"], bucket["used_percent"], bucket["resets_at"]): bucket
        for bucket in buckets
    }
    ordered = sorted(
        unique.values(),
        key=lambda item: (
            ("session", "weekly", "monthly", "other").index(item["period"]),
            ("overall", "model").index(item["scope"]),
            item["used_percent"],
            item["resets_at"] or 0,
        ),
    )
    return {
        "schema_version": SCHEMA_VERSION,
        "provider": provider,
        "observed_at": observed_at,
        "buckets": ordered,
    }


def normalize_openai(raw: Any, observed_at: int) -> dict[str, Any]:
    result = _object(_object(raw).get("result"))
    limits = _object(result.get("rateLimitsByLimitId"))
    buckets: list[dict[str, Any]] = []
    for limit_id, raw_limit in limits.items():
        limit = _object(raw_limit)
        name_value = limit.get("limitName")
        if not isinstance(name_value, str) or not name_value:
            name_value = limit_id
        name = name_value.lower() if isinstance(name_value, str) else ""
        scope = "overall" if name == "codex" else "model"
        for slot in ("primary", "secondary"):
            window_value = limit.get(slot)
            if window_value is None:
                continue
            window = _object(window_value)
            if window.get("usedPercent") is None or window.get("windowDurationMins") is None:
                continue
            buckets.append(
                _bucket(
                    _period_from_minutes(window["windowDurationMins"]),
                    scope,
                    _percent(window["usedPercent"]),
                    _epoch(window.get("resetsAt")),
                )
            )
    return _protocol("openai", observed_at, buckets)


def normalize_ollama(raw: Any, observed_at: int) -> dict[str, Any]:
    limits = _object(_object(raw).get("limits"))
    buckets: list[dict[str, Any]] = []
    for name, raw_limit in limits.items():
        limit = _object(raw_limit)
        if limit.get("usage") is None:
            continue
        buckets.append(
            _bucket(_period_from_name(name), "overall", _percent(limit["usage"], fraction=True), None)
        )
    return _protocol("ollama", observed_at, buckets)


def _anthropic_period(limit: dict[str, Any]) -> str:
    group = limit.get("group")
    if isinstance(group, str) and group.lower() in {"session", "weekly", "monthly", "other"}:
        return group.lower()
    kind = limit.get("kind")
    if kind in {"five_hour", "session"}:
        return "session"
    if kind in {"seven_day", "weekly", "weekly_all", "weekly_scoped"}:
        return "weekly"
    if kind == "monthly":
        return "monthly"
    return "other"


def normalize_anthropic(raw: Any, _observed_at: int | None = None) -> dict[str, Any]:
    snapshot = _object(_object(raw).get("cachedUsageUtilization"))
    fetched_ms = _epoch(snapshot.get("fetchedAtMs"))
    if fetched_ms is None:
        raise _fail("invalid-response")
    observed_at = fetched_ms // 1000
    utilization = _object(snapshot.get("utilization"))
    buckets: list[dict[str, Any]] = []
    limits_value = utilization.get("limits")
    if limits_value is not None:
        for raw_limit in _array(limits_value):
            limit = _object(raw_limit)
            if limit.get("percent") is None:
                continue
            buckets.append(
                _bucket(
                    _anthropic_period(limit),
                    "model" if limit.get("kind") == "weekly_scoped" else "overall",
                    _percent(limit["percent"]),
                    _rfc3339_epoch(limit.get("resets_at")),
                )
            )
    if not buckets:
        for key, period in (("five_hour", "session"), ("seven_day", "weekly")):
            value = utilization.get(key)
            if value is None:
                continue
            legacy = _object(value)
            if legacy.get("utilization") is None:
                continue
            buckets.append(
                _bucket(
                    period,
                    "overall",
                    _percent(legacy["utilization"]),
                    _rfc3339_epoch(legacy.get("resets_at")),
                )
            )
    return _protocol("anthropic", observed_at, buckets)


NORMALIZERS = {
    "anthropic": normalize_anthropic,
    "openai": normalize_openai,
    "ollama": normalize_ollama,
}


def _read_json_bytes(stream: Any, limit: int = MAX_RAW_BYTES) -> Any:
    data = stream.read(limit + 1)
    if len(data) > limit:
        raise _fail("output-too-large")
    try:
        return json.loads(data)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise _fail("invalid-response") from error


def _terminate_group(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=0.5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=0.5)
            except subprocess.TimeoutExpired:
                pass
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream is not None:
            stream.close()


class _JsonRpcReader:
    def __init__(self, process: subprocess.Popen[bytes]):
        if process.stdout is None:
            raise _fail("unavailable")
        self.process = process
        self.fd = process.stdout.fileno()
        self.buffer = bytearray()
        self.total = 0

    def response(self, request_id: int, deadline: float) -> dict[str, Any]:
        while True:
            while b"\n" in self.buffer:
                line, _, remainder = self.buffer.partition(b"\n")
                self.buffer = bytearray(remainder)
                if not line.strip():
                    continue
                try:
                    value = json.loads(line)
                except (json.JSONDecodeError, UnicodeDecodeError):
                    continue
                if isinstance(value, dict) and value.get("id") == request_id:
                    return value
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise _fail("timeout")
            readable, _, _ = select.select([self.fd], [], [], remaining)
            if not readable:
                raise _fail("timeout")
            chunk = os.read(self.fd, 4096)
            if not chunk:
                raise _fail("invalid-response")
            self.total += len(chunk)
            if self.total > MAX_RAW_BYTES:
                raise _fail("output-too-large")
            self.buffer.extend(chunk)


def probe_openai(*, codex_bin: str | None = None, timeout: float = DEFAULT_TIMEOUT) -> dict[str, Any]:
    executable = codex_bin or shutil.which("codex")
    if not executable:
        raise _fail("unavailable")
    try:
        process = subprocess.Popen(
            [executable, "app-server"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError as error:
        raise _fail("unavailable") from error
    try:
        if process.stdin is None:
            raise _fail("unavailable")
        reader = _JsonRpcReader(process)
        deadline = time.monotonic() + timeout

        def send(message: dict[str, Any]) -> None:
            try:
                process.stdin.write(json.dumps(message, separators=(",", ":")).encode() + b"\n")
                process.stdin.flush()
            except (BrokenPipeError, OSError) as error:
                raise _fail("unavailable") from error

        send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "claude-sandbox-usage-probe",
                        "title": "claude-sandbox-usage-probe",
                        "version": "1.0.0",
                    }
                },
            }
        )
        initialized = reader.response(1, deadline)
        if "error" in initialized:
            raise _fail("unavailable")
        send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
        send({"jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read", "params": {}})
        raw = reader.response(2, deadline)
        if "error" in raw:
            raise _fail("unavailable")
        return normalize_openai(raw, int(time.time()))
    finally:
        _terminate_group(process)


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req: Any, fp: Any, code: int, msg: str, headers: Any, newurl: str) -> None:
        raise urllib.error.HTTPError(req.full_url, code, "redirect denied", headers, fp)


def _ollama_key(auth_path: Path | None = None) -> str:
    key = os.environ.get("OLLAMA_API_KEY", "")
    if key:
        return key
    data_home = Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local/share")))
    path = auth_path or data_home / "opencode/auth.json"
    try:
        if path.stat().st_size > MAX_RAW_BYTES:
            raise _fail("unavailable")
        with path.open("rb") as stream:
            auth = _read_json_bytes(stream)
        value = _object(_object(auth).get("ollama-cloud")).get("key")
    except (OSError, ProbeError) as error:
        raise _fail("unavailable") from error
    if not isinstance(value, str) or not value:
        raise _fail("unavailable")
    return value


def probe_ollama(
    *,
    auth_path: Path | None = None,
    url: str = OLLAMA_USAGE_URL,
    timeout: float = DEFAULT_TIMEOUT,
) -> dict[str, Any]:
    request = urllib.request.Request(
        url,
        headers={"Authorization": f"Bearer {_ollama_key(auth_path)}", "Accept": "application/json"},
        method="GET",
    )
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect)
    try:
        with opener.open(request, timeout=timeout) as response:
            raw = _read_json_bytes(response)
    except ProbeError:
        raise
    except (OSError, urllib.error.URLError, urllib.error.HTTPError) as error:
        raise _fail("unavailable") from error
    return normalize_ollama(raw, int(time.time()))


def _write_private_state(path: Path, cwd: Path) -> None:
    path.mkdir(mode=0o700, parents=True)
    state = {
        "hasCompletedOnboarding": True,
        "projects": {str(cwd): {"hasTrustDialogAccepted": True}},
    }
    target = path / ".claude.json"
    target.write_text(json.dumps(state, separators=(",", ":")), encoding="utf-8")
    target.chmod(0o600)


def _load_anthropic_state(path: Path) -> dict[str, Any] | None:
    try:
        if path.stat().st_size > MAX_RAW_BYTES:
            return None
        with path.open("rb") as stream:
            value = _read_json_bytes(stream)
    except (OSError, ProbeError):
        return None
    try:
        normalize_anthropic(value)
    except ProbeError:
        return None
    return _object(value)


def probe_anthropic(
    *,
    claude_bin: str | None = None,
    secure_storage: Path = Path("/root/.claude"),
    timeout: float = DEFAULT_TIMEOUT,
) -> dict[str, Any]:
    executable = claude_bin or shutil.which("claude")
    if not executable or not secure_storage.is_dir():
        raise _fail("unavailable")
    with tempfile.TemporaryDirectory(prefix="claude-usage-probe-") as temporary:
        private_home = Path(temporary) / "home"
        private_state = Path(temporary) / "state"
        empty_cwd = Path(temporary) / "workspace"
        private_home.mkdir(mode=0o700)
        empty_cwd.mkdir(mode=0o700)
        _write_private_state(private_state, empty_cwd)
        state_path = private_state / ".claude.json"
        environment = os.environ.copy()
        for name in (
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_VERTEX",
        ):
            environment.pop(name, None)
        environment.update(
            {
                "HOME": str(private_home),
                "CLAUDE_CONFIG_DIR": str(private_state),
                "CLAUDE_SECURESTORAGE_CONFIG_DIR": str(secure_storage),
                "DISABLE_AUTOUPDATER": "1",
                "NO_COLOR": "1",
            }
        )
        master, slave = pty.openpty()
        fcntl.ioctl(
            slave,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", TUI_ROWS, TUI_COLUMNS, 0, 0),
        )
        command = [
            executable,
            "--safe-mode",
            "--permission-mode",
            "plan",
            "--tools",
            "",
            "--no-chrome",
        ]
        try:
            process = subprocess.Popen(
                command,
                cwd=empty_cwd,
                env=environment,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                start_new_session=True,
                close_fds=True,
            )
        except OSError as error:
            os.close(master)
            os.close(slave)
            raise _fail("unavailable") from error
        os.close(slave)
        os.set_blocking(master, False)
        deadline = time.monotonic() + timeout
        output_bytes = 0
        prompt_seen = False
        prompt_tail = b""
        sent_usage = False
        try:
            while time.monotonic() < deadline:
                readable, _, _ = select.select([master], [], [], 0.1)
                if readable:
                    try:
                        chunk = os.read(master, 4096)
                    except OSError:
                        chunk = b""
                    output_bytes += len(chunk)
                    if output_bytes > MAX_TUI_BYTES:
                        raise _fail("output-too-large")
                    if chunk:
                        prompt_seen = prompt_seen or b"\xe2\x9d\xaf" in prompt_tail + chunk
                        prompt_tail = (prompt_tail + chunk)[-2:]
                if not sent_usage and prompt_seen:
                    os.write(master, b"/usage\r")
                    sent_usage = True
                state = _load_anthropic_state(state_path)
                if state is not None:
                    try:
                        os.write(master, b"\x1b/exit\r")
                    except OSError:
                        pass
                    return normalize_anthropic(state)
                if process.poll() is not None:
                    raise _fail("unavailable")
            raise _fail("timeout")
        finally:
            try:
                os.close(master)
            except OSError:
                pass
            _terminate_group(process)


def normalize_from_stdin(provider: str) -> dict[str, Any]:
    raw = _read_json_bytes(sys.stdin.buffer)
    return NORMALIZERS[provider](raw, int(time.time()))


def main(argv: list[str]) -> int:
    try:
        if len(argv) == 2 and argv[0] == "--normalize" and argv[1] in PROVIDERS:
            result = normalize_from_stdin(argv[1])
        elif len(argv) == 1 and argv[0] in PROVIDERS:
            result = {
                "anthropic": probe_anthropic,
                "openai": probe_openai,
                "ollama": probe_ollama,
            }[argv[0]]()
        else:
            raise _fail("invalid-invocation")
        sys.stdout.write(json.dumps(result, separators=(",", ":")) + "\n")
        return 0
    except ProbeError as error:
        sys.stderr.write(f"usage-probe:{error.reason}\n")
        return 1
    except Exception:
        sys.stderr.write("usage-probe:internal-error\n")
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
