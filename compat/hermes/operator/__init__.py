"""Supported Hermes entrypoint for the Pohunek operator plugin."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

from .hooks import HookReporter, callbacks
from .policy import PolicyError, load_policy
from .cli import CliError
from .tools import TOOL_SCHEMAS, Tools


# The installer replaces this exact token with one absolute Python string literal.
# Keeping it in a dedicated constant prevents accidental profile-relative lookup.
POLICY_PATH = __POHUNEK_POLICY_PATH__
_SKILL_PATH = Path(__file__).parent / "skills" / "pohunek" / "SKILL.md"
_MAX_STATUS_FAILURES = 1_000_000
_STATUS: dict[str, Any] = {"state": "not_registered", "failure_count": 0}


def integration_status() -> dict[str, Any]:
    """Return the bounded, payload-free bootstrap state used by doctor."""
    return dict(_STATUS)


def _status(state: str, *, failed: bool = False) -> None:
    _STATUS["state"] = state
    if failed:
        _STATUS["failure_count"] = min(int(_STATUS["failure_count"]) + 1, _MAX_STATUS_FAILURES)


def register(ctx: Any) -> None:
    """Register only supported API surfaces; policy or CLI failures disable tools."""
    if not all(callable(getattr(ctx, name, None)) for name in ("register_tool", "register_hook", "register_skill")):
        _status("unsupported_context", failed=True)
        return
    reporter = HookReporter()
    # Hooks have no subprocess dependency and remain best effort on CLI mismatch.
    for name, callback in callbacks(reporter).items():
        ctx.register_hook(name, callback)
    try:
        policy = load_policy(POLICY_PATH)
    except PolicyError:
        _status("policy_invalid", failed=True)
        return
    tools = Tools(policy, os.environ.get("POHUNEK_SESSION_ID"))
    try:
        tools.verify_cli()
    except CliError:
        _status("cli_incompatible", failed=True)
        return
    handlers = tools.handlers()
    for name, handler in handlers.items():
        ctx.register_tool(
            name=name,
            toolset="pohunek",
            schema=TOOL_SCHEMAS[name],
            handler=handler,
            description=TOOL_SCHEMAS[name]["description"],
        )
    # Generated separately by the documentation pipeline; never synthesize it here.
    if all(name in handlers for name in tools.read_names) and _SKILL_PATH.is_file():
        ctx.register_skill("pohunek", _SKILL_PATH)
    _status("ready")
