"""Small, payload-safe diagnostics helpers for the Pohunek Hermes plugin."""

from __future__ import annotations

import re
from typing import Any


_SECRET = re.compile(r"(?i)(token|secret|password|authorization|api[_-]?key|access[_-]?key)\s*[=:]\s*(?:bearer\s+)?[^\s,;]+")
_BEARER = re.compile(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]+")
_PATH = re.compile(r"(?:file://)?/(?:[^\s/]+/){1,}[^\s/]+")
_MAX_DETAIL = 240


def diagnostic(value: Any) -> str:
    """Return a bounded redaction suitable for a tool error, never a payload."""
    text = str(value).replace("\n", " ").replace("\r", " ")
    text = _SECRET.sub(r"\1=<redacted>", text)
    text = _BEARER.sub("Bearer <redacted>", text)
    text = _PATH.sub("<path>", text)
    return text[:_MAX_DETAIL]


def tool_error(code: str, detail: str = "") -> dict[str, Any]:
    """Build the stable, intentionally terse tool error shape."""
    result: dict[str, Any] = {"ok": False, "error": {"code": code}}
    if detail:
        result["error"]["detail"] = diagnostic(detail)
    return result
