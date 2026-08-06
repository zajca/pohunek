"""One bounded JSON-only subprocess runner used by every Pohunek tool."""

from __future__ import annotations

import collections
import json
import os
import selectors
import signal
import subprocess
import threading
import time
from dataclasses import dataclass
from typing import Any

from .policy import Policy
from .redact import diagnostic


_STDERR_BYTES = 4096
_EXIT_GRACE_SECONDS = 0.2
_READ_CHUNK_BYTES = 64 * 1024
_METRIC_CAPACITY = 64
# JSON uses six ASCII bytes for the most expensive escaped input byte, such as
# a NUL encoded as ``\u0000``. Keep the capture cap valid for every bounded
# terminal and diff string the CLI may place in a successful envelope.
_JSON_ESCAPE_BYTES_PER_INPUT_BYTE = 6
# Standard base64 serializes each three raw bytes as four ASCII bytes; the
# final partial group still occupies one full output quantum.
_BASE64_INPUT_QUANTUM_BYTES = 3
_BASE64_OUTPUT_QUANTUM_BYTES = 4
# This must match Pohunek's pinned public `MAX_CONTROL_LINE_BYTES` contract.
# Low delegated-tool payload limits cannot reject an otherwise valid typed CLI
# result whose metadata already occupies a public control line.
_PUBLIC_CONTROL_LINE_BYTES = 1024 * 1024
# Pohunek's closed typed `--json` surface pretty-serializes daemon results.
# Reuse the six-byte JSON-escape budget as a conservative pinned expansion
# factor so a valid near-limit daemon collection remains readable at low tool
# limits. Re-audit this compatibility contract whenever typed shapes or the
# CLI serializer changes.
_PUBLIC_TYPED_PRETTY_EXPANSION_FACTOR = _JSON_ESCAPE_BYTES_PER_INPUT_BYTE
_PUBLIC_TYPED_PRETTY_FLOOR_BYTES = (
    _PUBLIC_CONTROL_LINE_BYTES * _PUBLIC_TYPED_PRETTY_EXPANSION_FACTOR
)
# Successful CLI envelopes include protocol metadata, cursor fields, result
# keys, and JSON punctuation. A fixed reserve keeps those fields bounded
# without treating policy output limits as a raw stdout-byte limit.
_RESPONSE_ENVELOPE_HEADROOM_BYTES = 16 * 1024
_PATH_ENV_KEYS = (
    "HOME", "XDG_RUNTIME_DIR", "XDG_STATE_HOME",
    "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME",
)
_LOCALE_ENV_KEYS = ("LANG", "LC_ALL")
_ORIGIN_ENV = ("POHUNEK_SESSION_ID", "POHUNEK_DAEMON_ID")


class CliError(RuntimeError):
    """Typed failure from the fixed Pohunek CLI surface."""

    def __init__(self, code: str, detail: str = "") -> None:
        super().__init__(detail)
        self.code = code
        self.detail = diagnostic(detail)


@dataclass(frozen=True)
class Invocation:
    """Fixed command suffix plus optional untrusted stdin text."""

    argv: tuple[str, ...]
    stdin: str | None = None


class CliRunner:
    """Execute fixed Pohunek argv arrays with bounded process-group cleanup."""

    def __init__(self, policy: Policy) -> None:
        self._policy = policy
        self._semaphore = threading.BoundedSemaphore(policy.max_concurrency)
        self.metrics: collections.deque[dict[str, Any]] = collections.deque(maxlen=_METRIC_CAPACITY)

    def run(self, invocation: Invocation) -> Any:
        if not self._semaphore.acquire(blocking=False):
            raise CliError("plugin_busy")
        started = time.monotonic()
        status = "ok"
        try:
            return self._run(invocation)
        except CliError as error:
            status = error.code
            raise
        except BaseException:
            status = "plugin_cancelled"
            raise
        finally:
            self._semaphore.release()
            self.metrics.append({
                "duration_ms": int((time.monotonic() - started) * 1000),
                "command": invocation.argv[0] if invocation.argv else "invalid",
                "status": status,
            })

    def verify_compatibility(self) -> None:
        """Prove the installed CLI speaks the policy's protocol before tools exist."""
        self.run(Invocation(("doctor", "--json")))

    def _run(self, invocation: Invocation) -> Any:
        if not invocation.argv or any(not isinstance(item, str) or not item for item in invocation.argv):
            raise CliError("plugin_invalid_request")
        if invocation.stdin is not None and not isinstance(invocation.stdin, str):
            raise CliError("plugin_invalid_request")
        input_bytes = invocation.stdin.encode("utf-8") if invocation.stdin is not None else b""
        process: subprocess.Popen[bytes] | None = None
        try:
            process = subprocess.Popen(
                [self._policy.pohunek_cli, *invocation.argv], stdin=subprocess.PIPE,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=False,
                env=_minimal_env(), close_fds=True, start_new_session=True,
            )
            stdout, stderr = self._collect(process, input_bytes)
            try:
                returncode = process.wait(timeout=_EXIT_GRACE_SECONDS)
            except subprocess.TimeoutExpired as error:
                raise CliError("plugin_timeout") from error
        except OSError as error:
            if process is None:
                raise CliError("pohunek_cli_unavailable", str(error)) from error
            self._terminate(process)
            raise CliError("pohunek_cli_failed", str(error)) from error
        except BaseException:
            if process is not None:
                self._terminate(process)
            raise
        document = self._parse_document(stdout)
        envelope = self._validate_envelope(document)
        has_ok = "ok" in envelope
        has_err = "err" in envelope
        if returncode == 0:
            if not has_ok or has_err:
                raise CliError("pohunek_cli_invalid_envelope")
            return envelope["ok"]
        if not has_err or has_ok:
            raise CliError("pohunek_cli_invalid_envelope")
        error = envelope["err"]
        if not isinstance(error, dict) or not isinstance(error.get("code"), str):
            raise CliError("pohunek_cli_invalid_envelope")
        raise CliError(error["code"])

    def _collect(self, process: subprocess.Popen[bytes], input_bytes: bytes) -> tuple[bytes, bytes]:
        """Pump all pipes incrementally and stop before either capture becomes unbounded."""
        if process.stdin is None or process.stdout is None or process.stderr is None:
            raise CliError("pohunek_cli_failed")
        streams = (("stdin", process.stdin), ("stdout", process.stdout), ("stderr", process.stderr))
        selector = selectors.DefaultSelector()
        try:
            for kind, stream in streams:
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_WRITE if kind == "stdin" else selectors.EVENT_READ, kind)
            stdout = bytearray()
            stderr = bytearray()
            offset = 0
            deadline = time.monotonic() + self._policy.tool_timeout_ms / 1000
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise CliError("plugin_timeout")
                for key, _mask in selector.select(min(remaining, 0.05)):
                    stream = key.fileobj
                    kind = key.data
                    if kind == "stdin":
                        if offset >= len(input_bytes):
                            selector.unregister(stream)
                            stream.close()
                            continue
                        try:
                            offset += os.write(stream.fileno(), input_bytes[offset:])
                        except BlockingIOError:
                            continue
                        if offset >= len(input_bytes):
                            selector.unregister(stream)
                            stream.close()
                        continue
                    try:
                        chunk = os.read(stream.fileno(), _READ_CHUNK_BYTES)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(stream)
                        stream.close()
                        continue
                    target, cap = (stdout, _stdout_wire_cap(self._policy)) if kind == "stdout" else (stderr, _STDERR_BYTES)
                    if len(target) + len(chunk) > cap:
                        raise CliError("plugin_output_limit_exceeded")
                    target.extend(chunk)
            return bytes(stdout), bytes(stderr)
        finally:
            selector.close()

    def _parse_document(self, stdout: bytes) -> dict[str, Any]:
        try:
            document = json.loads(stdout.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise CliError("pohunek_cli_invalid_json") from error
        if not isinstance(document, dict):
            raise CliError("pohunek_cli_invalid_envelope")
        return document

    def _validate_envelope(self, document: dict[str, Any]) -> dict[str, Any]:
        if set(document) - {"cli_version", "protocol", "ok", "err"}:
            raise CliError("pohunek_cli_invalid_envelope")
        protocol = document.get("protocol")
        if not isinstance(protocol, dict):
            raise CliError("pohunek_cli_invalid_envelope")
        minimum, maximum = protocol.get("minimum"), protocol.get("maximum")
        if isinstance(minimum, bool) or isinstance(maximum, bool) or not isinstance(minimum, int) or not isinstance(maximum, int) or minimum > maximum:
            raise CliError("pohunek_cli_invalid_envelope")
        if maximum < self._policy.protocol_min or minimum > self._policy.protocol_max:
            raise CliError("pohunek_cli_incompatible")
        if "ok" not in document and "err" not in document:
            raise CliError("pohunek_cli_invalid_envelope")
        return document

    @staticmethod
    def _terminate(process: subprocess.Popen[bytes]) -> None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=_EXIT_GRACE_SECONDS)
        except (OSError, subprocess.TimeoutExpired):
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except OSError:
                pass
            try:
                process.wait(timeout=_EXIT_GRACE_SECONDS)
            except (OSError, subprocess.TimeoutExpired):
                pass
        finally:
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream is not None:
                    try:
                        stream.close()
                    except OSError:
                        pass


def _minimal_env() -> dict[str, str]:
    env = {key: os.environ[key] for key in _LOCALE_ENV_KEYS if key in os.environ}
    for key in _PATH_ENV_KEYS:
        value = os.environ.get(key, "")
        if value and os.path.isabs(value):
            env[key] = value
    origin = {key: os.environ.get(key, "") for key in _ORIGIN_ENV}
    if all(_safe_origin_value(value) for value in origin.values()):
        env.update(origin)
    env["PATH"] = "/usr/bin:/bin"
    return env


def _stdout_wire_cap(policy: Policy) -> int:
    """Return the bounded serialized stdout ceiling for one typed CLI response."""
    base64_output_bytes = _base64_wire_bytes(policy.max_output_bytes)
    escaped_output_bytes = policy.max_output_bytes * _JSON_ESCAPE_BYTES_PER_INPUT_BYTE
    escaped_screen_bytes = policy.max_screen_bytes * _JSON_ESCAPE_BYTES_PER_INPUT_BYTE
    payload_bytes = max(
        _PUBLIC_TYPED_PRETTY_FLOOR_BYTES,
        base64_output_bytes,
        escaped_output_bytes,
        escaped_screen_bytes,
    )
    return payload_bytes + _RESPONSE_ENVELOPE_HEADROOM_BYTES


def _base64_wire_bytes(raw_bytes: int) -> int:
    """Return standard-base64 bytes including the required partial final group."""
    groups = (raw_bytes + _BASE64_INPUT_QUANTUM_BYTES - 1) // _BASE64_INPUT_QUANTUM_BYTES
    return groups * _BASE64_OUTPUT_QUANTUM_BYTES


def _safe_origin_value(value: str) -> bool:
    return bool(value) and len(value) <= 256 and "/" not in value and "\x00" not in value and all(ord(character) >= 32 for character in value)
