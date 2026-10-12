"""Core-owned, in-process reporting over Pohunek's local Unix sockets."""

from __future__ import annotations

import json
import os
import socket
import struct
import sys
import time
from datetime import datetime, timedelta, timezone
from typing import Any


_RESPONSE_BYTES = 4096
_REQUEST_BYTES = 4096
_IDENTITY_TTL_SECONDS = 30
_MAX_HOOK_TIMEOUT_SECONDS = 1.0
_DEFAULT_HOOK_TIMEOUT_SECONDS = 0.25
_MAX_NATIVE_ID_CHARS = 512
_MAX_FAILURE_COUNT = 1_000_000
_PROVIDER_LABELS = {
    "claude": "Claude",
    "codex": "Codex",
    "hermes": "Hermes",
    "shell": "Shell",
}
_PUBLIC_METHODS = frozenset((
    "session.report_native_id",
    "session.report_agent",
    "notification.create",
    "session.release_agent",
))
_ACTIVITIES = frozenset(("working", "blocked", "idle"))
_ATTENTION = frozenset((
    ("approval_required", "action_required"),
    ("turn_completed", "success"),
    ("agent_blocked", "warning"),
    ("error", "error"),
))

# Darwin exposes process start time through `proc_pidinfo` with the
# `PROC_PIDTBSDINFO` flavor, which fills a 136-byte `struct proc_bsdinfo`.
# The daemon and CLI encode the same value from this kernel structure.
_DARWIN_LIBSYSTEM = "/usr/lib/libSystem.B.dylib"
_DARWIN_PROC_PIDTBSDINFO = 3
_DARWIN_BSDINFO_SIZE = 136
_DARWIN_BSDINFO_PID_OFFSET = 12
_DARWIN_BSDINFO_START_OFFSET = 120
_MICROSECONDS_PER_SECOND = 1_000_000
_START_IDENTITY_LIMIT = 1 << 64


class Reporter:
    """Send fixed-shape reports without affecting the owning agent's turn."""

    def __init__(self, provider: str, environ: dict[str, str] | None = None) -> None:
        if not isinstance(provider, str) or provider not in _PROVIDER_LABELS:
            raise ValueError("unsupported Pohunek reporter provider")
        env = os.environ if environ is None else environ
        self._provider = provider
        self._provider_label = _PROVIDER_LABELS[provider]
        self._session_id = env.get("POHUNEK_SESSION_ID", "") if env.get("POHUNEK_ENV") == "1" else ""
        self._worker_instance_id = env.get("POHUNEK_WORKER_INSTANCE_ID", "")
        self._worker_socket = _local_socket(env.get("POHUNEK_WORKER_SOCKET_PATH", ""))
        self._daemon_socket = _local_socket(env.get("POHUNEK_SOCKET_PATH", ""))
        self._protocol = _positive_int(env.get("POHUNEK_PROTOCOL_VERSION"))
        self._host_local_source_id = _source_id(env.get("POHUNEK_DAEMON_ID", ""), provider)
        self._deadline = _deadline(env.get("POHUNEK_HOOK_TIMEOUT_MS"))
        self._pid = os.getpid()
        self._start_identity = _process_start_identity(self._pid)
        self._sequence = 0
        self.failures = 0

    @property
    def active(self) -> bool:
        return bool(self._session_id and self._worker_instance_id and self._start_identity is not None)

    def identity(self, reference: str | None) -> None:
        """Report a bounded native reference to the worker, then public API."""
        if not self.active or not isinstance(reference, str) or not 0 < len(reference) <= _MAX_NATIVE_ID_CHARS:
            return
        self._safe(self._report_identity, reference)

    def activity(self, state: str) -> None:
        """Report one of the fixed agent activity states."""
        if not self.active or not isinstance(state, str) or state not in _ACTIVITIES:
            return
        self._safe(self._report_activity, state)

    def attention(self, kind: str, severity: str) -> None:
        """Report one of the fixed Pohunek attention events."""
        if not self.active or not isinstance(kind, str) or not isinstance(severity, str) or (kind, severity) not in _ATTENTION:
            return
        self._safe(self._report_attention, kind, severity)

    def release(self) -> None:
        """Release the current process identity using worker-first routing."""
        if not self.active:
            return
        sequence = self._next_sequence()
        private = {
            "type": "identity_release", "runtime_id": self._worker_instance_id, "provider": self._provider,
            "pid": self._pid, "start_identity": self._start_identity, "sequence": sequence,
        }
        if self._safe_worker(private):
            return
        self._safe(self._send_public, "session.release_agent", {
            "session_id": self._session_id, "source": f"pohunek:{self._provider}", "agent": self._provider,
            "seq": sequence,
        })

    def _report_identity(self, native_id: str) -> None:
        sequence = self._next_sequence()
        expires = (datetime.now(timezone.utc) + timedelta(seconds=_IDENTITY_TTL_SECONDS)).isoformat().replace("+00:00", "Z")
        private = {
            "type": "identity_report", "runtime_id": self._worker_instance_id, "provider": self._provider,
            "pid": self._pid, "start_identity": self._start_identity, "sequence": sequence,
            "expires_at": expires, "reference_kind": "id", "native_reference": native_id,
        }
        if self._safe_worker(private):
            return
        self._safe(self._send_public, "session.report_native_id", {
            "session_id": self._session_id, "worker_instance_id": self._worker_instance_id,
            "agent": self._provider, "pid": self._pid, "pid_start_identity": str(self._start_identity),
            "sequence": str(sequence), "expires_at": expires, "native_session_id": native_id,
        })

    def _report_activity(self, state: str) -> None:
        self._safe(self._send_public, "session.report_agent", {
            "session_id": self._session_id, "source": f"pohunek:{self._provider}", "agent": self._provider,
            "activity": state, "seq": self._next_sequence(), "pid": self._pid,
        })

    def _report_attention(self, kind: str, severity: str) -> None:
        self._safe(self._send_public, "notification.create", {
            "source": {
                "provider": self._provider, "provider_event": kind,
                "host_local_source_id": self._host_local_source_id,
            },
            "kind": kind, "severity": severity, "title": f"{self._provider_label} session update",
            "body": f"{self._provider_label} session activity changed.", "metadata": {},
            "session_id": self._session_id, "agent_kind": self._provider,
            "dedupe_key": f"attention:{self._session_id}",
        })

    def _send_worker(self, message: dict[str, Any]) -> bool:
        if not self._worker_socket:
            return False
        try:
            response = self._send(self._worker_socket, message)
            if isinstance(response, dict) and response.get("ok") is True and "err" not in response:
                # A pending launch claim is durably owned and retried by the worker.
                # Public fallback must not race that immutable launch decision.
                return True
            self._record_failure()
            return False
        except Exception:
            self._record_failure()
            return False

    def _send_public(self, method: str, params: dict[str, Any]) -> None:
        if not isinstance(method, str) or method not in _PUBLIC_METHODS:
            self._record_failure()
            return
        if not self._daemon_socket or self._protocol is None:
            return
        request = {
            "v": {"minimum": self._protocol, "maximum": self._protocol},
            "id": f"hermes-hook:{self._sequence}:{method.rsplit('.', 1)[-1]}",
            "method": method, "params": params,
        }
        try:
            response = self._send(self._daemon_socket, request)
            if not isinstance(response, dict) or "ok" not in response or "err" in response:
                self._record_failure()
        except Exception:
            self._record_failure()

    def _send(self, endpoint: str, message: dict[str, Any]) -> Any:
        encoded = (json.dumps(message, separators=(",", ":")) + "\n").encode("utf-8")
        if len(encoded) > _REQUEST_BYTES:
            self._record_failure()
            return None
        deadline = time.monotonic() + self._deadline
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(self._remaining(deadline))
            client.connect(endpoint)
            client.settimeout(self._remaining(deadline))
            client.sendall(encoded)
            response = bytearray()
            while len(response) < _RESPONSE_BYTES:
                client.settimeout(self._remaining(deadline))
                chunk = client.recv(_RESPONSE_BYTES - len(response))
                if not chunk:
                    break
                response.extend(chunk)
                if b"\n" in chunk:
                    break
        return json.loads(bytes(response).split(b"\n", 1)[0]) if response else None

    @staticmethod
    def _remaining(deadline: float) -> float:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise socket.timeout("report deadline exceeded")
        return remaining

    def _next_sequence(self) -> int:
        self._sequence += 1
        return self._sequence

    def _safe(self, callback: Any, *args: Any) -> None:
        try:
            callback(*args)
        except Exception:
            self._record_failure()

    def _safe_worker(self, message: dict[str, Any]) -> bool:
        try:
            return self._send_worker(message)
        except Exception:
            self._record_failure()
            return False

    def _record_failure(self) -> None:
        self.failures = min(self.failures + 1, _MAX_FAILURE_COUNT)


def _local_socket(value: str) -> str | None:
    return value if value.startswith("/") and "\x00" not in value else None


def _source_id(value: str, provider: str) -> str:
    if value and len(value) <= _MAX_NATIVE_ID_CHARS and "/" not in value and "\x00" not in value and all(ord(character) >= 32 for character in value):
        return value
    return provider


def _positive_int(value: str | None) -> int | None:
    try:
        parsed = int(value) if value else None
    except ValueError:
        return None
    return parsed if parsed and parsed > 0 else None


def _deadline(value: str | None) -> float:
    try:
        milliseconds = int(value) if value else int(_DEFAULT_HOOK_TIMEOUT_SECONDS * 1000)
    except ValueError:
        milliseconds = int(_DEFAULT_HOOK_TIMEOUT_SECONDS * 1000)
    return max(0.001, min(milliseconds / 1000, _MAX_HOOK_TIMEOUT_SECONDS))


def _process_start_identity(pid: int) -> int | None:
    """Return the platform's opaque process-generation value for `pid`."""
    if sys.platform == "darwin":
        return _darwin_process_start_identity(pid)
    return _linux_process_start_identity(pid)


def _linux_process_start_identity(pid: int) -> int | None:
    try:
        with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
            fields = handle.read().rsplit(")", 1)[1].split()
        return int(fields[19])
    except (OSError, IndexError, ValueError):
        return None


def _darwin_process_start_identity(pid: int) -> int | None:
    try:
        import ctypes

        libsystem = ctypes.CDLL(_DARWIN_LIBSYSTEM, use_errno=True)
        proc_pidinfo = libsystem.proc_pidinfo
        proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
        proc_pidinfo.restype = ctypes.c_int
        buffer = ctypes.create_string_buffer(_DARWIN_BSDINFO_SIZE)
        written = proc_pidinfo(pid, _DARWIN_PROC_PIDTBSDINFO, 0, buffer, _DARWIN_BSDINFO_SIZE)
        if written != _DARWIN_BSDINFO_SIZE:
            return None
        return _darwin_start_identity_from_bsdinfo(buffer.raw, pid)
    except (AttributeError, ImportError, OSError):
        return None


def _darwin_start_identity_from_bsdinfo(raw: bytes, pid: int) -> int | None:
    """Decode `struct proc_bsdinfo` bytes and reject a different process."""
    if len(raw) != _DARWIN_BSDINFO_SIZE:
        return None
    (reported_pid,) = struct.unpack_from("<I", raw, _DARWIN_BSDINFO_PID_OFFSET)
    seconds, microseconds = struct.unpack_from("<QQ", raw, _DARWIN_BSDINFO_START_OFFSET)
    if reported_pid != pid or microseconds >= _MICROSECONDS_PER_SECOND:
        return None
    identity = seconds * _MICROSECONDS_PER_SECOND + microseconds
    return identity if identity < _START_IDENTITY_LIMIT else None
