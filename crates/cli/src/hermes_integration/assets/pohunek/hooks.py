"""Best-effort local-only Hermes lifecycle reporting."""

from __future__ import annotations

import json
import os
import socket
import time
from datetime import datetime, timedelta, timezone
from typing import Any, Callable


_RESPONSE_BYTES = 4096
_IDENTITY_TTL_SECONDS = 30
_MAX_HOOK_TIMEOUT_SECONDS = 1.0
_DEFAULT_HOOK_TIMEOUT_SECONDS = 0.25
_MAX_NATIVE_ID_CHARS = 512
_MAX_FAILURE_COUNT = 1_000_000


class HookReporter:
    """Emit fixed-shape Unix-socket reports without affecting a Hermes turn."""

    def __init__(self, environ: dict[str, str] | None = None) -> None:
        env = os.environ if environ is None else environ
        self._session_id = env.get("POHUNEK_SESSION_ID", "") if env.get("POHUNEK_ENV") == "1" else ""
        self._runtime_id = env.get("POHUNEK_RUNTIME_ID", "")
        self._worker_socket = _local_socket(env.get("POHUNEK_WORKER_SOCKET_PATH", ""))
        self._daemon_socket = _local_socket(env.get("POHUNEK_SOCKET_PATH", ""))
        self._protocol = _positive_int(env.get("POHUNEK_PROTOCOL_VERSION"))
        self._host_local_source_id = _source_id(env.get("POHUNEK_DAEMON_ID", ""))
        self._deadline = _deadline(env.get("POHUNEK_HOOK_TIMEOUT_MS"))
        self._pid = os.getpid()
        self._start_identity = _process_start_identity(self._pid)
        self._sequence = 0
        self.failures = 0

    @property
    def active(self) -> bool:
        return bool(self._session_id and self._runtime_id and self._start_identity is not None)

    def on_session_start(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        self._safe(self._identity, args, kwargs)

    def pre_llm_call(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        self._safe(self._identity, args, kwargs)
        self._safe(self._activity, "working")

    def pre_approval_request(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        del args, kwargs
        self._safe(self._activity, "blocked")
        self._safe(self._attention, "approval_required", "action_required")

    def post_approval_response(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        del args, kwargs
        # The daemon's activity projector resolves the matching approval when
        # the same agent returns to working; no raw approval payload is sent.
        self._safe(self._activity, "working")

    def post_llm_call(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        del args, kwargs
        self._safe(self._activity, "idle")
        self._safe(self._attention, "turn_completed", "success")

    def on_session_end(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        self._safe(self._activity, "idle")
        outcome = _outcome(args, kwargs)
        if outcome == "interrupted":
            self._safe(self._attention, "agent_blocked", "warning")
        elif outcome == "failed":
            self._safe(self._attention, "error", "error")

    def on_session_finalize(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        del args, kwargs
        if not self.active:
            return
        sequence = self._next_sequence()
        private = {
            "type": "identity_release", "runtime_id": self._runtime_id, "provider": "hermes",
            "pid": self._pid, "start_identity": self._start_identity, "sequence": sequence,
        }
        if self._safe_worker(private):
            return
        self._safe(self._send_public, "session.release_agent", {
            "session_id": self._session_id, "source": "pohunek:hermes", "agent": "hermes",
            "seq": sequence,
        })

    def _identity(self, args: dict[str, Any] | None, kwargs: dict[str, Any]) -> None:
        if not self.active:
            return
        native_id = _native_id(args, kwargs)
        if native_id is None:
            return
        sequence = self._next_sequence()
        expires = (datetime.now(timezone.utc) + timedelta(seconds=_IDENTITY_TTL_SECONDS)).isoformat().replace("+00:00", "Z")
        private = {
            "type": "identity_report", "runtime_id": self._runtime_id, "provider": "hermes",
            "pid": self._pid, "start_identity": self._start_identity, "sequence": sequence,
            "expires_at": expires, "reference_kind": "id", "native_reference": native_id,
        }
        if self._safe_worker(private):
            return
        self._safe(self._send_public, "session.report_native_id", {
            "session_id": self._session_id, "runtime_id": self._runtime_id, "agent": "hermes",
            "pid": self._pid, "pid_start_identity": str(self._start_identity), "sequence": str(sequence),
            "expires_at": expires, "native_session_id": native_id,
        })

    def _activity(self, activity: str) -> None:
        if not self.active:
            return
        self._safe(self._send_public, "session.report_agent", {
            "session_id": self._session_id, "source": "pohunek:hermes", "agent": "hermes",
            "activity": activity, "seq": self._next_sequence(), "pid": self._pid,
        })

    def _attention(self, kind: str, severity: str) -> None:
        if not self.active:
            return
        self._send_public("notification.create", {
            "source": {"provider": "hermes", "provider_event": kind, "host_local_source_id": self._host_local_source_id},
            "kind": kind, "severity": severity, "title": "Hermes session update",
            "body": "Hermes session activity changed.", "metadata": {}, "session_id": self._session_id,
            "agent_kind": "hermes", "dedupe_key": f"attention:{self._session_id}",
        })

    def _send_worker(self, message: dict[str, Any]) -> bool:
        if not self._worker_socket:
            return False
        try:
            response = self._send(self._worker_socket, message)
            if isinstance(response, dict) and response.get("ok") is True and "err" not in response:
                return True
            self._record_failure()
            return False
        except Exception:
            self._record_failure()
            return False

    def _send_public(self, method: str, params: dict[str, Any]) -> None:
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
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(self._deadline)
            client.connect(endpoint)
            client.sendall(encoded)
            response = client.recv(_RESPONSE_BYTES)
        return json.loads(response.splitlines()[0]) if response else None

    def _next_sequence(self) -> int:
        self._sequence += 1
        return self._sequence

    def _safe(self, callback: Callable[..., Any], *args: Any) -> None:
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


def callbacks(reporter: HookReporter) -> dict[str, Callable[..., None]]:
    """Return only the supported Hermes hook callback names."""
    return {
        "on_session_start": reporter.on_session_start,
        "pre_llm_call": reporter.pre_llm_call,
        "pre_approval_request": reporter.pre_approval_request,
        "post_approval_response": reporter.post_approval_response,
        "post_llm_call": reporter.post_llm_call,
        "on_session_end": reporter.on_session_end,
        "on_session_finalize": reporter.on_session_finalize,
    }


def _local_socket(value: str) -> str | None:
    return value if value.startswith("/") and "\x00" not in value else None


def _source_id(value: str) -> str:
    if value and len(value) <= _MAX_NATIVE_ID_CHARS and "/" not in value and "\x00" not in value and all(ord(character) >= 32 for character in value):
        return value
    return "hermes"


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
    try:
        with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
            fields = handle.read().rsplit(")", 1)[1].split()
        return int(fields[19])
    except (OSError, IndexError, ValueError):
        return None


def _native_id(args: dict[str, Any] | None, kwargs: dict[str, Any]) -> str | None:
    for source in (args, kwargs):
        if isinstance(source, dict):
            value = source.get("session_id")
            if isinstance(value, str) and 0 < len(value) <= _MAX_NATIVE_ID_CHARS:
                return value
    return None


def _outcome(args: dict[str, Any] | None, kwargs: dict[str, Any]) -> str:
    for source in (args, kwargs):
        if isinstance(source, dict):
            if source.get("interrupted") is True:
                return "interrupted"
            if source.get("failed") is True:
                return "failed"
            if source.get("completed") is False and source.get("interrupted") is False:
                return "failed"
            value = source.get("outcome") or source.get("status")
            if value in {"interrupted", "failed", "completed"}:
                return value
    return "completed"
