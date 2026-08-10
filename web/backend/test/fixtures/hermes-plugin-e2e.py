"""Run the embedded Pohunek plugin tools against an isolated real daemon.

This fixture deliberately imports the production package with ``importlib`` instead
of adding its asset directory to ``sys.path``.  The one rendered policy-path
substitution is the same transformation performed by the installer; every other
file is copied byte-for-byte into an isolated temporary package.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import shutil
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


_PLUGIN_PACKAGE_NAME = "pohunek_e2e_plugin"
_MARKER = "pohunek-hermes-plugin-e2e-marker"
_REMOTE_MARKER = "pohunek-hermes-plugin-remote-marker"
_NATIVE_REFERENCE = "hermes-e2e-native"
_GAP_COMPLETE_MARKER = "pohunek-hermes-gap-output-complete"
_GAP_OUTPUT_BYTES = 11_000_000
_MAX_RETRIES = 200
_RETRY_DELAY_SECONDS = 0.05


class FixtureError(RuntimeError):
    """A payload-free assertion failure from this controlled fixture."""


def parse_args() -> argparse.Namespace:
    """Parse only explicit, test-owned paths and IDs."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--assets-dir", required=True)
    parser.add_argument("--policy", required=True)
    parser.add_argument("--project-id", required=True)
    parser.add_argument("--remote-host", required=True)
    return parser.parse_args()


def load_plugin(assets_dir: Path, policy_path: Path) -> tuple[Any, Path]:
    """Materialize exactly one isolated package with the installer substitution."""
    source = assets_dir / "pohunek"
    if not source.is_dir() or not policy_path.is_absolute():
        raise FixtureError("fixture input is invalid")
    temporary = Path(tempfile.mkdtemp(prefix="pohunek-plugin-e2e-", dir=policy_path.parent))
    package = temporary / _PLUGIN_PACKAGE_NAME
    shutil.copytree(source, package)
    init_path = package / "__init__.py"
    source_text = init_path.read_text(encoding="utf-8")
    needle = "POLICY_PATH = __POHUNEK_POLICY_PATH__"
    if source_text.count(needle) != 1:
        raise FixtureError("plugin policy placeholder is invalid")
    init_path.write_text(source_text.replace(needle, f"POLICY_PATH = {str(policy_path)!r}"), encoding="utf-8")

    spec = importlib.util.spec_from_file_location(
        _PLUGIN_PACKAGE_NAME,
        init_path,
        submodule_search_locations=[str(package)],
    )
    if spec is None or spec.loader is None:
        raise FixtureError("plugin import specification is unavailable")
    module = importlib.util.module_from_spec(spec)
    sys.modules[_PLUGIN_PACKAGE_NAME] = module
    spec.loader.exec_module(module)
    return module, temporary


def invoke(handlers: dict[str, Any], name: str, args: dict[str, Any]) -> Any:
    """Call one registered production handler and retain no terminal payload."""
    response = json.loads(handlers[name](args))
    if response.get("ok") is not True or "result" not in response:
        error = response.get("error")
        code = error.get("code") if isinstance(error, dict) else "unknown"
        raise FixtureError(f"tool {name} failed with {code}")
    return response["result"]


def require_string(value: Any, field: str) -> str:
    """Require one nonempty wire identifier without exposing its value."""
    if not isinstance(value, str) or not value:
        raise FixtureError(f"missing {field}")
    return value


def canonical_u64(value: Any, field: str) -> str:
    """Require the protocol's canonical unsigned decimal string shape."""
    if not isinstance(value, str) or not value or (value != "0" and value.startswith("0")):
        raise FixtureError(f"missing {field}")
    if any(character not in "0123456789" for character in value):
        raise FixtureError(f"missing {field}")
    if len(value) > 20 or (len(value) == 20 and value > "18446744073709551615"):
        raise FixtureError(f"missing {field}")
    return value


def runtime_from(session: Any) -> tuple[str, str]:
    """Extract one logical session's current durable runtime identity."""
    if not isinstance(session, dict):
        raise FixtureError("session result is invalid")
    runtime = session.get("runtime")
    if not isinstance(runtime, dict):
        raise FixtureError("session runtime is missing")
    runtime_id = require_string(runtime.get("runtime_id"), "runtime_id")
    generation = canonical_u64(runtime.get("runtime_generation"), "runtime_generation")
    return runtime_id, generation


def inspect_until_live(handlers: dict[str, Any], session_id: str) -> tuple[dict[str, Any], str, str]:
    """Wait only for the worker's bounded startup transition."""
    for _ in range(_MAX_RETRIES):
        session = invoke(handlers, "pohunek_session_get", {"session": session_id})
        if isinstance(session, dict):
            try:
                runtime_id, generation = runtime_from(session)
            except FixtureError:
                time.sleep(_RETRY_DELAY_SECONDS)
                continue
            if session.get("id") == session_id:
                return session, runtime_id, generation
        time.sleep(_RETRY_DELAY_SECONDS)
    raise FixtureError("session did not expose a live runtime")


def inspect_until_native(handlers: dict[str, Any], session_id: str) -> tuple[dict[str, Any], str, str]:
    """Wait for the controlled worker-private Hermes identity report."""
    for _ in range(_MAX_RETRIES):
        session, runtime_id, generation = inspect_until_live(handlers, session_id)
        if session.get("native_session_id") == _NATIVE_REFERENCE:
            return session, runtime_id, generation
        time.sleep(_RETRY_DELAY_SECONDS)
    raise FixtureError("Hermes native identity was not recorded")


def inspect_until_terminal(handlers: dict[str, Any], session_id: str) -> dict[str, Any]:
    """Wait for the controlled initial Hermes process to exit naturally."""
    for _ in range(_MAX_RETRIES):
        session = invoke(handlers, "pohunek_session_get", {"session": session_id})
        if isinstance(session, dict) and session.get("state") in {"done", "failed", "stopped"}:
            if session.get("native_session_id") != _NATIVE_REFERENCE:
                raise FixtureError("terminal Hermes session lost its native reference")
            runtime_from(session)
            return session
        time.sleep(_RETRY_DELAY_SECONDS)
    raise FixtureError("initial Hermes runtime did not become terminal")


def wait_for_screen_marker(
    handlers: dict[str, Any], session_id: str, runtime_id: str, generation: str, marker: str,
) -> None:
    """Wait until the worker has processed one controlled terminal marker."""
    for _ in range(_MAX_RETRIES):
        screen = invoke(handlers, "pohunek_session_screen", {"session": session_id})
        if not isinstance(screen, dict) or screen.get("session_id") != session_id:
            raise FixtureError("screen logical ID mismatch while waiting for output")
        if screen.get("runtime_id") != runtime_id or screen.get("runtime_generation") != generation:
            raise FixtureError("screen runtime ID mismatch while waiting for output")
        text = screen.get("text")
        if isinstance(text, str) and marker in text:
            return
        time.sleep(_RETRY_DELAY_SECONDS)
    raise FixtureError("controlled output completion marker was not observed")


def require_session_result(value: Any, session_id: str, field: str) -> dict[str, Any]:
    """Require a matching logical ID and a complete durable runtime."""
    if not isinstance(value, dict) or value.get("id") != session_id:
        raise FixtureError(f"{field} logical ID mismatch")
    runtime_from(value)
    return value


def require_output_shape(value: Any, session_id: str, runtime_id: str, generation: str) -> str:
    """Validate output cursors without retaining or returning terminal text."""
    if not isinstance(value, dict) or value.get("session_id") != session_id:
        raise FixtureError("output logical ID mismatch")
    if value.get("runtime_id") != runtime_id or value.get("runtime_generation") != generation:
        raise FixtureError("output runtime ID mismatch")
    for field in ("history_start_offset", "start_offset", "next_offset", "runtime_end_offset"):
        canonical_u64(value.get(field), field)
    if not isinstance(value.get("has_more"), bool) or not isinstance(value.get("timed_out"), bool):
        raise FixtureError("output pagination shape is invalid")
    return value["next_offset"]


def settle_output(
    handlers: dict[str, Any], session_id: str, runtime_id: str, generation: str, cursor: str,
) -> str:
    """Advance past startup bytes and prove one bounded output no-change result."""
    for _ in range(20):
        settled = invoke(handlers, "pohunek_session_output", {
            "session": session_id, "runtime_id": runtime_id, "runtime_generation": generation,
            "after_offset": cursor, "max_bytes": 4096, "wait_ms": 50,
        })
        next_cursor = require_output_shape(settled, session_id, runtime_id, generation)
        if settled.get("timed_out") is True:
            if next_cursor != cursor or settled.get("text") != "":
                raise FixtureError("timed-out output changed its cursor")
            return cursor
        cursor = next_cursor
    raise FixtureError("session output did not become quiescent")


def require_wait(value: Any, session_id: str, reason: str, cursor: str | None = None) -> None:
    """Require one exact wait outcome and its logical/runtime snapshot."""
    if not isinstance(value, dict) or value.get("reason") != reason:
        raise FixtureError(f"wait did not return {reason}")
    session = value.get("session")
    if not isinstance(session, dict) or session.get("id") != session_id:
        raise FixtureError("wait logical ID mismatch")
    runtime_from(session)
    if cursor is not None and canonical_u64(value.get("output_offset"), "output_offset") != cursor:
        raise FixtureError("wait no-change cursor mismatch")


def assert_origin_denials(plugin: Any, policy: Any, session_id: str) -> None:
    """Prove every pre-subprocess origin mutation denial through real handlers."""
    handlers = plugin.Tools(policy, session_id).handlers()
    calls = {
        "pohunek_session_send": {"session": session_id, "input": "x"},
        "pohunek_session_resume": {"session": session_id},
        "pohunek_session_fork": {"session": session_id},
        "pohunek_session_resize": {"session": session_id, "cols": 80, "rows": 24},
        "pohunek_session_rename": {"session": session_id, "name": "blocked"},
        "pohunek_session_set_metadata": {"session": session_id, "metadata": {"label": "blocked"}},
        "pohunek_session_stop": {"session": session_id},
        "pohunek_session_remove": {"session": session_id},
    }
    for name, arguments in calls.items():
        response = json.loads(handlers[name](arguments))
        error = response.get("error")
        if not isinstance(error, dict) or error.get("code") != "plugin_self_target_denied":
            raise FixtureError(f"origin denial failed for {name}")


def run() -> dict[str, Any]:
    """Exercise a full-mode non-origin shell session through the real CLI surface."""
    args = parse_args()
    plugin, temporary = load_plugin(Path(args.assets_dir), Path(args.policy))
    try:
        return run_plugin(plugin, args)
    finally:
        shutil.rmtree(temporary, ignore_errors=True)


def run_plugin(plugin: Any, args: argparse.Namespace) -> dict[str, Any]:
    """Run the assertions while the isolated import package is available."""
    policy = plugin.load_policy(plugin.POLICY_PATH)
    tools = plugin.Tools(policy, None)
    handlers = tools.handlers()
    if len(handlers) != 16:
        raise FixtureError("full policy did not register the complete tool surface")

    hosts = invoke(handlers, "pohunek_hosts", {})
    if hosts != {"hosts": ["local", args.remote_host], "wildcard": False, "discovery_performed": False}:
        raise FixtureError("policy hosts result is invalid")

    started = invoke(
        handlers,
        "pohunek_session_start",
        {
            "agent_profile": "shell",
            "name": "hermes-plugin-e2e",
            "worktree": {
                "project": {"id": args.project_id},
                "branch": "hermes-plugin-e2e-worktree",
                "base_branch": "main",
            },
            "cols": 80,
            "rows": 24,
        },
    )
    if not isinstance(started, dict):
        raise FixtureError("start result is invalid")
    session_id = require_string(started.get("id"), "session_id")
    started_runtime, started_generation = runtime_from(started)
    inspected, runtime_id, generation = inspect_until_live(handlers, session_id)
    require_session_result(inspected, session_id, "inspect")
    if runtime_id != started_runtime or generation != started_generation:
        raise FixtureError("start and inspect runtime identities differ")

    sessions = invoke(handlers, "pohunek_sessions", {"filters": {"id": session_id}})
    if not isinstance(sessions, dict) or not any(
        isinstance(item, dict) and item.get("id") == session_id
        for item in sessions.get("sessions", [])
    ):
        raise FixtureError("list did not return the started logical ID")

    screen = invoke(handlers, "pohunek_session_screen", {"session": session_id})
    if not isinstance(screen, dict) or screen.get("session_id") != session_id:
        raise FixtureError("screen logical ID mismatch")
    if screen.get("runtime_id") != runtime_id or screen.get("runtime_generation") != generation:
        raise FixtureError("screen runtime ID mismatch")
    canonical_u64(screen.get("watermark"), "watermark")
    if screen.get("truncated") is not False or not isinstance(screen.get("text"), str):
        raise FixtureError("screen truncation shape is invalid")

    initial = invoke(
        handlers,
        "pohunek_session_output",
        {
            "session": session_id,
            "runtime_id": runtime_id,
            "runtime_generation": generation,
            "max_bytes": 4096,
        },
    )
    cursor = require_output_shape(initial, session_id, runtime_id, generation)
    cursor = settle_output(handlers, session_id, runtime_id, generation, cursor)

    timed_out = invoke(
        handlers,
        "pohunek_session_wait",
        {
            "session": session_id,
            "runtime_id": runtime_id,
            "runtime_generation": generation,
            "after_output_offset": cursor,
            "timeout_ms": 25,
        },
    )
    require_wait(timed_out, session_id, "timeout", cursor)

    invoke(handlers, "pohunek_session_send", {"session": session_id, "input": f"printf '{_MARKER}\\n'\n"})
    woken = invoke(
        handlers,
        "pohunek_session_wait",
        {
            "session": session_id,
            "runtime_id": runtime_id,
            "runtime_generation": generation,
            "after_output_offset": cursor,
            "timeout_ms": 2_000,
        },
    )
    require_wait(woken, session_id, "output_advanced")

    incremental = invoke(
        handlers,
        "pohunek_session_output",
        {
            "session": session_id,
            "runtime_id": runtime_id,
            "runtime_generation": generation,
            "after_offset": cursor,
            "max_bytes": 4096,
        },
    )
    cursor = require_output_shape(incremental, session_id, runtime_id, generation)
    if _MARKER not in incremental.get("text", ""):
        raise FixtureError("incremental output did not contain the controlled marker")

    remote_sessions = invoke(handlers, "pohunek_sessions", {"host": args.remote_host, "filters": {"id": session_id}})
    if not isinstance(remote_sessions, dict) or not any(
        isinstance(item, dict) and item.get("id") == session_id
        for item in remote_sessions.get("sessions", [])
    ):
        raise FixtureError("remote list did not return the local logical ID")
    invoke(handlers, "pohunek_session_send", {
        "host": args.remote_host, "session": session_id, "input": f"printf '{_REMOTE_MARKER}\\n'\n",
    })
    remote_wait = invoke(handlers, "pohunek_session_wait", {
        "host": args.remote_host, "session": session_id, "runtime_id": runtime_id,
        "runtime_generation": generation, "after_output_offset": cursor, "timeout_ms": 2_000,
    })
    require_wait(remote_wait, session_id, "output_advanced")
    remote_screen = invoke(handlers, "pohunek_session_screen", {"host": args.remote_host, "session": session_id})
    if not isinstance(remote_screen, dict) or remote_screen.get("session_id") != session_id:
        raise FixtureError("remote screen logical ID mismatch")

    gap_command = (
        f"/usr/bin/head -c {_GAP_OUTPUT_BYTES} /dev/zero | "
        "/usr/bin/tr '\\000' G; "
        "printf '\\n%s%s\\n' 'pohunek-hermes-' 'gap-output-complete'; "
        "read -r _pohunek_gap_release\n"
    )
    invoke(handlers, "pohunek_session_send", {"session": session_id, "input": gap_command})
    wait_for_screen_marker(
        handlers, session_id, runtime_id, generation, _GAP_COMPLETE_MARKER,
    )
    gap_page = None
    for _ in range(_MAX_RETRIES):
        candidate = invoke(handlers, "pohunek_session_output", {
            "session": session_id, "runtime_id": runtime_id, "runtime_generation": generation,
            "after_offset": "0", "max_bytes": 262_144,
        })
        if isinstance(candidate, dict) and isinstance(candidate.get("gap"), dict):
            gap_page = candidate
            break
        time.sleep(_RETRY_DELAY_SECONDS)
    if gap_page is None:
        raise FixtureError("bounded output history did not expose a deterministic gap")
    gap_cursor = require_output_shape(gap_page, session_id, runtime_id, generation)
    gap = gap_page["gap"]
    if canonical_u64(gap.get("start_offset"), "gap.start") != "0":
        raise FixtureError("gap did not begin at the requested cursor")
    canonical_u64(gap.get("end_offset"), "gap.end")
    recovered = invoke(handlers, "pohunek_session_output", {
        "session": session_id, "runtime_id": runtime_id, "runtime_generation": generation,
        "after_offset": gap_cursor, "max_bytes": 4096,
    })
    require_output_shape(recovered, session_id, runtime_id, generation)
    if recovered.get("start_offset") != gap_cursor or recovered.get("gap") is not None:
        raise FixtureError("gap recovery did not continue from the returned cursor")
    invoke(handlers, "pohunek_session_send", {"session": session_id, "input": "\n"})

    invoke(handlers, "pohunek_session_resize", {"session": session_id, "cols": 100, "rows": 30})
    invoke(handlers, "pohunek_session_rename", {"session": session_id, "name": "hermes-plugin-renamed"})
    invoke(handlers, "pohunek_session_set_metadata", {"session": session_id, "metadata": {"label": "e2e"}})
    renamed, current_runtime, current_generation = inspect_until_live(handlers, session_id)
    if renamed.get("name") != "hermes-plugin-renamed":
        raise FixtureError("rename did not persist")
    if current_runtime != runtime_id or current_generation != generation:
        raise FixtureError("unexpected runtime change during mutation")

    diff = invoke(handlers, "pohunek_session_diff", {"session": session_id})
    if not isinstance(diff, dict) or not isinstance(diff.get("diff"), str) or not isinstance(diff.get("truncated"), bool):
        raise FixtureError("diff result shape is invalid")

    assert_origin_denials(plugin, policy, session_id)
    stopped = invoke(handlers, "pohunek_session_stop", {"session": session_id})
    if not isinstance(stopped, dict) or stopped.get("stopped") is not True:
        raise FixtureError("stop result is invalid")
    removed = invoke(handlers, "pohunek_session_remove", {"session": session_id})
    if not isinstance(removed, dict) or removed.get("removed") is not True:
        raise FixtureError("remove result is invalid")

    hermes_started = invoke(handlers, "pohunek_session_start", {
        "agent_profile": "hermes", "name": "hermes-runtime-e2e",
        "project": {"id": args.project_id}, "cols": 80, "rows": 24,
    })
    hermes_id = require_string(hermes_started.get("id") if isinstance(hermes_started, dict) else None, "Hermes session_id")
    hermes_session, hermes_runtime, hermes_generation = inspect_until_native(handlers, hermes_id)
    require_session_result(hermes_session, hermes_id, "Hermes start")
    hermes_output = invoke(handlers, "pohunek_session_output", {
        "session": hermes_id, "runtime_id": hermes_runtime,
        "runtime_generation": hermes_generation, "max_bytes": 4096,
    })
    old_cursor = require_output_shape(hermes_output, hermes_id, hermes_runtime, hermes_generation)
    forked = invoke(handlers, "pohunek_session_fork", {"session": hermes_id})
    if forked != {"fork_supported": False, "error": {"code": "agent_fork_unsupported"}}:
        raise FixtureError("Hermes fork was not returned as structured unsupported data")
    inspect_until_terminal(handlers, hermes_id)
    resumed = invoke(handlers, "pohunek_session_resume", {"session": hermes_id})
    if not isinstance(resumed, dict):
        raise FixtureError("Hermes resume result is invalid")
    require_session_result(resumed.get("session"), hermes_id, "Hermes resume")
    resumed_session, resumed_runtime, resumed_generation = inspect_until_native(handlers, hermes_id)
    if resumed_runtime == hermes_runtime or int(resumed_generation) <= int(hermes_generation):
        raise FixtureError("Hermes resume did not mint a new runtime identity")
    stale = invoke(handlers, "pohunek_session_wait", {
        "session": hermes_id, "runtime_id": hermes_runtime, "runtime_generation": hermes_generation,
        "after_output_offset": old_cursor, "timeout_ms": 2_000,
    })
    require_wait(stale, hermes_id, "runtime_changed")
    require_session_result(resumed_session, hermes_id, "Hermes runtime recovery")
    final_hermes_stop = invoke(handlers, "pohunek_session_stop", {"session": hermes_id})
    if not isinstance(final_hermes_stop, dict) or final_hermes_stop.get("stopped") is not True:
        raise FixtureError("resumed Hermes stop result is invalid")
    invoke(handlers, "pohunek_session_remove", {"session": hermes_id})

    return {
        "ok": True,
        "logical_id_present": True,
        "runtime_id_present": True,
        "tools_exercised": 16,
        "origin_denials": 8,
        "hermes_resume": True,
        "hermes_fork_unsupported": True,
        "output_gap_recovered": True,
        "remote_loopback": True,
    }


def main() -> int:
    """Emit exactly one bounded, payload-free JSON document."""
    try:
        print(json.dumps(run(), separators=(",", ":"), ensure_ascii=True))
        return 0
    except FixtureError as error:
        print(json.dumps({"ok": False, "error": str(error)[:160]}, separators=(",", ":"), ensure_ascii=True))
        return 1
    except Exception:
        print('{"ok":false,"error":"fixture_failed"}')
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
