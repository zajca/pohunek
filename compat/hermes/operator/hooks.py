"""Adapt the supported Hermes lifecycle callbacks to Pohunek reports."""

from __future__ import annotations

from typing import Any, Callable

from ._pohunek_reporter import Reporter


_MAX_NATIVE_ID_CHARS = 512
_MAX_FAILURE_COUNT = 1_000_000


class HookReporter:
    """Translate Hermes callbacks into the reporter's fixed operations."""

    def __init__(self, reporter: Reporter | None = None) -> None:
        self._reporter = reporter if reporter is not None else Reporter("hermes")
        self._callback_failures = 0

    @property
    def active(self) -> bool:
        return self._reporter.active

    @property
    def failures(self) -> int:
        return min(self._reporter.failures + self._callback_failures, _MAX_FAILURE_COUNT)

    def on_session_start(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        self._safe(self._reporter.identity, _native_id(args, kwargs))

    def pre_llm_call(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        self._safe(self._reporter.identity, _native_id(args, kwargs))
        self._safe(self._reporter.activity, "working")

    def pre_approval_request(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        del args, kwargs
        self._safe(self._reporter.activity, "blocked")
        self._safe(self._reporter.attention, "approval_required", "action_required")

    def post_approval_response(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        del args, kwargs
        # Returning to working resolves the daemon's matching approval.
        self._safe(self._reporter.activity, "working")

    def post_llm_call(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        del args, kwargs
        self._safe(self._reporter.activity, "idle")
        self._safe(self._reporter.attention, "turn_completed", "success")

    def on_session_end(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        self._safe(self._reporter.activity, "idle")
        outcome = _outcome(args, kwargs)
        if outcome == "interrupted":
            self._safe(self._reporter.attention, "agent_blocked", "warning")
        elif outcome == "failed":
            self._safe(self._reporter.attention, "error", "error")

    def on_session_finalize(self, args: dict[str, Any] | None = None, **kwargs: Any) -> None:
        del args, kwargs
        self._safe(self._reporter.release)

    def _safe(self, callback: Callable[..., Any], *args: Any) -> None:
        try:
            callback(*args)
        except Exception:
            # Hook reports are best effort and never affect a Hermes turn.
            self._callback_failures = min(self._callback_failures + 1, _MAX_FAILURE_COUNT)


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
            if isinstance(value, str) and value in {"interrupted", "failed", "completed"}:
                return value
    return "completed"
