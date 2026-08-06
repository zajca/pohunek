"""Complete typed, policy-bounded Hermes tools backed by the real Pohunek CLI."""

from __future__ import annotations

import json
import base64
import binascii
import unicodedata
from typing import Any, Callable

from .cli import CliError, CliRunner, Invocation
from .policy import Policy, valid_host
from .redact import tool_error


_MAX_RESULTS = 100
_MAX_INPUT_BYTES = 262_144
_MAX_NAME_CHARS = 256
_MAX_DIMENSION = 500
_MIN_DIMENSION = 1
_MAX_WAIT_MS = 8_000
_U64_MAX = "18446744073709551615"
_METADATA_KEYS = frozenset(("label", "issue", "priority"))
_READ = (
    "pohunek_hosts", "pohunek_sessions", "pohunek_session_get", "pohunek_session_screen",
    "pohunek_session_output", "pohunek_session_wait", "pohunek_session_diff",
)
_MANAGE = (
    "pohunek_session_start", "pohunek_session_send", "pohunek_session_resume",
    "pohunek_session_fork", "pohunek_session_resize", "pohunek_session_rename",
    "pohunek_session_set_metadata",
)
_FULL = ("pohunek_session_stop", "pohunek_session_remove")
_ORIGIN_MUTATIONS = frozenset(("stop", "resume", "remove", "fork", "resize", "set_metadata", "rename", "input"))


_STRING_SCHEMA = {"type": "string"}
_INTEGER_SCHEMA = {"type": "integer"}
_U64_DECIMAL_SCHEMA = {"type": "string", "pattern": "^(0|[1-9][0-9]*)$", "maxLength": 20}
_HOST_SESSION_PROPERTIES = {"host": _STRING_SCHEMA, "session": _STRING_SCHEMA}


def _schema(description: str, properties: dict[str, Any], required: list[str] | None = None) -> dict[str, Any]:
    return {
        "name": "",
        "description": description,
        "parameters": {"type": "object", "properties": properties, "required": required or [], "additionalProperties": False},
    }


TOOL_SCHEMAS = {
    "pohunek_hosts": _schema("List Pohunek hosts permitted by the installed policy.", {}),
    "pohunek_sessions": _schema("List bounded Pohunek sessions on one permitted host.", {"host": _STRING_SCHEMA, "filters": {"type": "object", "properties": {key: _STRING_SCHEMA for key in ("id", "state", "activity", "agent", "project")}, "additionalProperties": False}}),
    "pohunek_session_get": _schema("Inspect one session by stable ID or exact name.", _HOST_SESSION_PROPERTIES, ["session"]),
    "pohunek_session_screen": _schema("Read a bounded rendered terminal screen.", _HOST_SESSION_PROPERTIES, ["session"]),
    "pohunek_session_output": _schema("Read bounded incremental terminal output with cursors.", {**_HOST_SESSION_PROPERTIES, "runtime_id": _STRING_SCHEMA, "runtime_generation": _U64_DECIMAL_SCHEMA, "after_offset": _U64_DECIMAL_SCHEMA, "max_bytes": _INTEGER_SCHEMA, "wait_ms": _INTEGER_SCHEMA}, ["session"]),
    "pohunek_session_wait": _schema("Wait for one bounded session state, activity, or output change.", {**_HOST_SESSION_PROPERTIES, "runtime_id": _STRING_SCHEMA, "runtime_generation": _U64_DECIMAL_SCHEMA, "after_updated_at": _STRING_SCHEMA, "after_terminal_watermark": _U64_DECIMAL_SCHEMA, "after_output_offset": _U64_DECIMAL_SCHEMA, "states": {"type": "array", "items": _STRING_SCHEMA}, "activities": {"type": "array", "items": _STRING_SCHEMA}, "timeout_ms": _INTEGER_SCHEMA}, ["session", "timeout_ms"]),
    "pohunek_session_diff": _schema("Read a bounded session worktree diff.", {**_HOST_SESSION_PROPERTIES, "base": _STRING_SCHEMA}, ["session"]),
    "pohunek_session_start": _schema("Start a session with a host-inventory agent profile and structured project or worktree selection.", {"host": _STRING_SCHEMA, "agent_profile": _STRING_SCHEMA, "name": _STRING_SCHEMA, "project": {"oneOf": [{"type": "object", "properties": {"id": _STRING_SCHEMA}, "required": ["id"], "additionalProperties": False}, {"type": "object", "properties": {"label": _STRING_SCHEMA}, "required": ["label"], "additionalProperties": False}]}, "worktree": {"type": "object", "properties": {"project": {"oneOf": [{"type": "object", "properties": {"id": _STRING_SCHEMA}, "required": ["id"], "additionalProperties": False}, {"type": "object", "properties": {"label": _STRING_SCHEMA}, "required": ["label"], "additionalProperties": False}]}, "branch": _STRING_SCHEMA, "base_branch": _STRING_SCHEMA}, "required": ["project"], "additionalProperties": False}, "cols": _INTEGER_SCHEMA, "rows": _INTEGER_SCHEMA, "initial_input": _STRING_SCHEMA}, ["agent_profile"]),
    "pohunek_session_send": _schema("Send bounded terminal input through stdin.", {**_HOST_SESSION_PROPERTIES, "input": _STRING_SCHEMA}, ["session", "input"]),
    "pohunek_session_resume": _schema("Resume a session with a valid native reference.", _HOST_SESSION_PROPERTIES, ["session"]),
    "pohunek_session_fork": _schema("Fork a session when its adapter supports native fork.", {**_HOST_SESSION_PROPERTIES, "name": _STRING_SCHEMA}, ["session"]),
    "pohunek_session_resize": _schema("Resize a managed terminal.", {**_HOST_SESSION_PROPERTIES, "cols": _INTEGER_SCHEMA, "rows": _INTEGER_SCHEMA}, ["session", "cols", "rows"]),
    "pohunek_session_rename": _schema("Set one logical session display name.", {**_HOST_SESSION_PROPERTIES, "name": _STRING_SCHEMA}, ["session", "name"]),
    "pohunek_session_set_metadata": _schema("Set or clear fixed public metadata fields.", {**_HOST_SESSION_PROPERTIES, "metadata": {"type": "object", "properties": {key: {"type": ["string", "null"]} for key in _METADATA_KEYS}, "additionalProperties": False}}, ["session", "metadata"]),
    "pohunek_session_stop": _schema("Stop an eligible non-origin live session.", _HOST_SESSION_PROPERTIES, ["session"]),
    "pohunek_session_remove": _schema("Remove an eligible non-origin logical session.", _HOST_SESSION_PROPERTIES, ["session"]),
}
for _tool_name, _tool_schema in TOOL_SCHEMAS.items():
    _tool_schema["name"] = _tool_name


_ALLOWED_ARGUMENTS = {
    name: frozenset(schema["parameters"]["properties"])
    for name, schema in TOOL_SCHEMAS.items()
}
_METHOD_TO_TOOL = {
    "hosts": "pohunek_hosts", "sessions": "pohunek_sessions", "session_get": "pohunek_session_get",
    "screen": "pohunek_session_screen", "output": "pohunek_session_output", "wait": "pohunek_session_wait",
    "diff": "pohunek_session_diff", "start": "pohunek_session_start", "send": "pohunek_session_send",
    "resume": "pohunek_session_resume", "fork": "pohunek_session_fork", "resize": "pohunek_session_resize",
    "rename": "pohunek_session_rename", "set_metadata": "pohunek_session_set_metadata",
    "stop": "pohunek_session_stop", "remove": "pohunek_session_remove",
}


class Tools:
    """Factories for registered handlers; every call returns exactly one JSON string."""

    def __init__(self, policy: Policy, origin_session_id: str | None) -> None:
        self._policy = policy
        self._origin = origin_session_id or ""
        self._runner = CliRunner(policy)

    @property
    def read_names(self) -> tuple[str, ...]:
        return _READ

    def verify_cli(self) -> None:
        """Fail registration closed when the installer-recorded CLI is incompatible."""
        self._runner.verify_compatibility()

    def handlers(self) -> dict[str, Callable[..., str]]:
        handlers: dict[str, Callable[..., str]] = {
            "pohunek_hosts": self._handle(self.hosts),
            "pohunek_sessions": self._handle(self.sessions),
            "pohunek_session_get": self._handle(self.session_get),
            "pohunek_session_screen": self._handle(self.screen),
            "pohunek_session_output": self._handle(self.output),
            "pohunek_session_wait": self._handle(self.wait),
            "pohunek_session_diff": self._handle(self.diff),
        }
        if self._policy.allows_manage():
            handlers.update({
                "pohunek_session_start": self._handle(self.start),
                "pohunek_session_send": self._handle(self.send),
                "pohunek_session_resume": self._handle(self.resume),
                "pohunek_session_fork": self._handle(self.fork),
                "pohunek_session_resize": self._handle(self.resize),
                "pohunek_session_rename": self._handle(self.rename),
                "pohunek_session_set_metadata": self._handle(self.set_metadata),
            })
        if self._policy.allows_full():
            handlers.update({
                "pohunek_session_stop": self._handle(self.stop),
                "pohunek_session_remove": self._handle(self.remove),
            })
        return handlers

    def hosts(self, args: dict[str, Any], **kwargs: Any) -> Any:
        del args, kwargs
        if "*" in self._policy.allowed_hosts:
            return {"hosts": ["*"], "wildcard": True, "discovery_performed": False}
        return {"hosts": sorted(self._policy.allowed_hosts), "wildcard": False, "discovery_performed": False}

    def sessions(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host = self._host(args)
        filters = _object(args.get("filters", {}), "filters")
        argv = ["--host", host, "session", "list", "--json"]
        for key in ("id", "state", "activity", "agent", "project"):
            if key in filters:
                value = _string(filters[key], key, _MAX_NAME_CHARS)
                argv.extend(("--filter", f"{key}={value}"))
        if set(filters) - {"id", "state", "activity", "agent", "project"}:
            raise CliError("plugin_invalid_request")
        return {"sessions": _bounded(self._runner.run(Invocation(tuple(argv))), _MAX_RESULTS)}

    def session_get(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._target(args)
        return self._runner.run(Invocation(("--host", host, "session", "inspect", target, "--json")))

    def screen(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._target(args)
        result = self._runner.run(Invocation(("--host", host, "session", "screen", target, "--json")))
        return _normalize_terminal(result, self._policy.max_screen_bytes)

    def output(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._target(args)
        maximum = _bounded_int(args.get("max_bytes", self._policy.max_output_bytes), 1, self._policy.max_output_bytes)
        argv = ["--host", host, "session", "output", target, "--max-bytes", str(maximum), "--json"]
        _runtime_args(argv, args)
        if "after_offset" in args:
            argv.extend(("--after-offset", _canonical_u64(args["after_offset"])))
        if "wait_ms" in args:
            if "after_offset" not in args:
                raise CliError("plugin_invalid_request")
            argv.extend(("--wait-ms", str(_bounded_int(args["wait_ms"], 1, _MAX_WAIT_MS))))
        if "after_offset" in args and "runtime_id" not in args:
            raise CliError("plugin_invalid_request")
        return _normalize_terminal(self._runner.run(Invocation(tuple(argv))), maximum, decode_output=True)

    def wait(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._target(args)
        timeout = _bounded_int(args.get("timeout_ms"), 1, _MAX_WAIT_MS)
        argv = ["--host", host, "session", "wait", target, "--timeout-ms", str(timeout), "--json"]
        _runtime_args(argv, args)
        if any(name in args for name in ("after_terminal_watermark", "after_output_offset")) and "runtime_id" not in args:
            raise CliError("plugin_invalid_request")
        for source, option in (("after_updated_at", "--after-updated-at"), ("after_terminal_watermark", "--after-terminal-watermark"), ("after_output_offset", "--after-output-offset")):
            if source in args:
                if source == "after_updated_at":
                    value = _string(args[source], source, _MAX_NAME_CHARS)
                else:
                    value = _canonical_u64(args[source])
                argv.extend((option, value))
        for state in _string_list(args.get("states", []), "states", 5, 32):
            argv.extend(("--state", state))
        for activity in _string_list(args.get("activities", []), "activities", 3, 32):
            argv.extend(("--activity", activity))
        return self._runner.run(Invocation(tuple(argv)))

    def diff(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._target(args)
        argv = ["--host", host, "session", "diff", target, "--json"]
        if "base" in args:
            argv.extend(("--base", _string(args["base"], "base", _MAX_NAME_CHARS)))
        return _normalize_terminal(self._runner.run(Invocation(tuple(argv))), self._policy.max_output_bytes)

    def start(self, args: dict[str, Any], **kwargs: Any) -> Any:
        initial = args.get("initial_input")
        validated_initial = _input(initial) if initial is not None else None
        host = self._host(args)
        agent = _string(args.get("agent_profile"), "agent_profile", _MAX_NAME_CHARS)
        project = args.get("project")
        worktree = args.get("worktree")
        if project is not None and worktree is not None:
            raise CliError("plugin_invalid_request")
        worktree_fields: dict[str, Any] | None = None
        if worktree is not None:
            worktree_fields = _object(worktree, "worktree")
            if set(worktree_fields) - {"project", "branch", "base_branch"} or "project" not in worktree_fields:
                raise CliError("plugin_invalid_request")
        if project is None and worktree is None:
            raise CliError("plugin_invalid_request")
        self._validate_inventory_agent(host, agent)
        argv = ["--host", host, "session", "new", "--agent", agent, "--json", "--yes"]
        if "name" in args:
            argv.extend(("--name", _string(args["name"], "name", _MAX_NAME_CHARS)))
        if project is not None:
            argv.extend(("--project", self._resolve_project(host, project)))
        if worktree_fields is not None:
            argv.extend(("--project", self._resolve_project(host, worktree_fields["project"])))
            for name, flag in (("branch", "--branch"), ("base_branch", "--base-branch")):
                if name in worktree_fields:
                    argv.extend((flag, _string(worktree_fields[name], f"worktree.{name}", _MAX_NAME_CHARS)))
        _dimensions(argv, args)
        if validated_initial is not None:
            argv.append("--input-stdin")
            return self._runner.run(Invocation(tuple(argv), validated_initial))
        return self._runner.run(Invocation(tuple(argv)))

    def send(self, args: dict[str, Any], **kwargs: Any) -> Any:
        text = _input(args.get("input"))
        host, target = self._mutation_target(args, "input")
        return self._runner.run(Invocation(("--host", host, "session", "input", target, "--stdin", "--json"), text))

    def resume(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._mutation_target(args, "resume")
        return self._runner.run(Invocation(("--host", host, "session", "resume", target, "--json")))

    def fork(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._mutation_target(args, "fork")
        argv = ["--host", host, "session", "fork", target, "--json"]
        if "name" in args:
            argv.extend(("--name", _string(args["name"], "name", _MAX_NAME_CHARS)))
        try:
            return self._runner.run(Invocation(tuple(argv)))
        except CliError as error:
            if error.code == "agent_fork_unsupported":
                return {"fork_supported": False, "error": {"code": error.code}}
            raise

    def resize(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._mutation_target(args, "resize")
        argv = ["--host", host, "session", "resize", target, "--json"]
        _dimensions(argv, args, required=True)
        return self._runner.run(Invocation(tuple(argv)))

    def rename(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._mutation_target(args, "rename")
        name = _string(args.get("name"), "name", _MAX_NAME_CHARS)
        return self._runner.run(Invocation(("--host", host, "session", "rename", target, name, "--json")))

    def set_metadata(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._mutation_target(args, "set_metadata")
        metadata = _object(args.get("metadata"), "metadata")
        if not metadata or set(metadata) - _METADATA_KEYS:
            raise CliError("plugin_invalid_request")
        argv = ["--host", host, "session", "metadata", target, "--json"]
        for key in sorted(metadata):
            value = metadata[key]
            if value is None:
                argv.extend(("--clear", key))
            else:
                argv.extend(("--set", f"{key}={_string(value, key, _MAX_NAME_CHARS)}"))
        return self._runner.run(Invocation(tuple(argv)))

    def stop(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._mutation_target(args, "stop")
        return self._runner.run(Invocation(("--host", host, "session", "stop", target, "--json")))

    def remove(self, args: dict[str, Any], **kwargs: Any) -> Any:
        host, target = self._mutation_target(args, "remove")
        return self._runner.run(Invocation(("--host", host, "session", "rm", target, "--json")))

    def _target(self, args: dict[str, Any]) -> tuple[str, str]:
        host = self._host(args)
        return host, _session_reference(args.get("session"))

    def _mutation_target(self, args: dict[str, Any], method: str) -> tuple[str, str]:
        host, requested = self._target(args)
        if method in _ORIGIN_MUTATIONS and requested == self._origin:
            raise CliError("plugin_self_target_denied")
        target = self._resolve_unique_target(host, requested)
        if method in _ORIGIN_MUTATIONS and target == self._origin:
            raise CliError("plugin_self_target_denied")
        return host, target

    def _resolve_unique_target(self, host: str, requested: str) -> str:
        sessions = self._runner.run(Invocation(("--host", host, "session", "list", "--json")))
        if not isinstance(sessions, list):
            raise CliError("pohunek_cli_invalid_envelope")
        ids = {entry.get("id") for entry in sessions if isinstance(entry, dict) and isinstance(entry.get("id"), str)}
        if requested in ids:
            return requested
        candidates = sorted(entry["id"] for entry in sessions if isinstance(entry, dict) and entry.get("name") == requested and isinstance(entry.get("id"), str))
        if len(candidates) == 1:
            return candidates[0]
        if len(candidates) > 1:
            raise CliError("plugin_ambiguous_session")
        raise CliError("plugin_session_not_found")

    def _resolve_project(self, host: str, selection: Any) -> str:
        project = _object(selection, "project")
        if set(project) not in ({"id"}, {"label"}):
            raise CliError("plugin_invalid_request")
        value = _string(next(iter(project.values())), "project", _MAX_NAME_CHARS)
        projects = self._runner.run(Invocation(("--host", host, "project", "list", "--json")))
        if not isinstance(projects, list):
            raise CliError("pohunek_cli_invalid_envelope")
        ids = {entry.get("id") for entry in projects if isinstance(entry, dict) and isinstance(entry.get("id"), str)}
        if "id" in project:
            if value not in ids:
                raise CliError("plugin_project_not_found")
            return value
        matches = sorted(entry["id"] for entry in projects if isinstance(entry, dict) and entry.get("label") == value and isinstance(entry.get("id"), str))
        if len(matches) == 1:
            return matches[0]
        if len(matches) > 1:
            raise CliError("plugin_ambiguous_project")
        raise CliError("plugin_project_not_found")

    def _validate_inventory_agent(self, host: str, agent: str) -> None:
        capabilities = self._runner.run(Invocation(("host", "inspect", host, "--json")))
        if not isinstance(capabilities, dict) or agent not in capabilities.get("supported_agents", []):
            raise CliError("plugin_agent_denied")
        runtimes = capabilities.get("runtimes", [])
        if not isinstance(runtimes, list):
            raise CliError("plugin_agent_denied")
        matching = [item for item in runtimes if isinstance(item, dict) and item.get("agent") == agent]
        if matching and (matching[0].get("available") is not True or matching[0].get("supported") is False):
            raise CliError("plugin_agent_denied")

    def _host(self, args: dict[str, Any]) -> str:
        host = args.get("host", "local")
        if not isinstance(host, str) or len(host) > _MAX_NAME_CHARS or not valid_host(host):
            raise CliError("plugin_host_denied")
        if "*" not in self._policy.allowed_hosts and host not in self._policy.allowed_hosts:
            raise CliError("plugin_host_denied")
        return host

    @staticmethod
    def _handle(function: Callable[..., Any]) -> Callable[..., str]:
        def handler(args: dict[str, Any] | None = None, **kwargs: Any) -> str:
            try:
                parsed = args if isinstance(args, dict) else {}
                tool_name = _METHOD_TO_TOOL.get(function.__name__)
                if tool_name is None or set(parsed) - _ALLOWED_ARGUMENTS[tool_name]:
                    raise CliError("plugin_invalid_request")
                return json.dumps({"ok": True, "result": function(parsed, **kwargs)}, ensure_ascii=False)
            except CliError as error:
                return json.dumps(tool_error(error.code, error.detail), ensure_ascii=False)
            except (TypeError, ValueError):
                return json.dumps(tool_error("plugin_invalid_request"), ensure_ascii=False)
        return handler


def _object(value: Any, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise CliError("plugin_invalid_request", f"{name} must be an object")
    return value


def _string(value: Any, name: str, maximum: int) -> str:
    if not isinstance(value, str) or not value or len(value) > maximum or "\x00" in value or any(ord(character) < 32 for character in value):
        raise CliError("plugin_invalid_request", f"{name} is invalid")
    return value


def _session_reference(value: Any) -> str:
    reference = _string(value, "session", _MAX_NAME_CHARS)
    if "/" in reference:
        raise CliError("plugin_invalid_request", "host-qualified session targets are not supported")
    return reference


def _string_list(value: Any, name: str, maximum_items: int, maximum_length: int) -> list[str]:
    if not isinstance(value, list) or len(value) > maximum_items:
        raise CliError("plugin_invalid_request")
    return [_string(item, name, maximum_length) for item in value]


def _bounded_int(value: Any, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise CliError("plugin_invalid_request")
    return value


def _canonical_u64(value: Any) -> str:
    """Validate and preserve one canonical unsigned decimal wire integer."""
    if not isinstance(value, str) or not value:
        raise CliError("plugin_invalid_request")
    if value == "0":
        return value
    if value[0] not in "123456789" or any(character not in "0123456789" for character in value):
        raise CliError("plugin_invalid_request")
    if len(value) > len(_U64_MAX) or (len(value) == len(_U64_MAX) and value > _U64_MAX):
        raise CliError("plugin_invalid_request")
    return value


def _input(value: Any) -> str:
    if (
        not isinstance(value, str)
        or not value
        or len(value) > _MAX_INPUT_BYTES
        or len(value.encode("utf-8")) > _MAX_INPUT_BYTES
        or any(
            unicodedata.category(character) == "Cc" and character not in "\n\r\t"
            for character in value
        )
    ):
        raise CliError("plugin_invalid_request")
    return value


def _dimensions(argv: list[str], args: dict[str, Any], required: bool = False) -> None:
    for name in ("cols", "rows"):
        if required or name in args:
            argv.extend((f"--{name}", str(_bounded_int(args.get(name), _MIN_DIMENSION, _MAX_DIMENSION))))


def _runtime_args(argv: list[str], args: dict[str, Any]) -> None:
    runtime_id = args.get("runtime_id")
    generation = args.get("runtime_generation")
    if (runtime_id is None) != (generation is None):
        raise CliError("plugin_invalid_request")
    if runtime_id is not None:
        argv.extend(("--runtime-id", _string(runtime_id, "runtime_id", _MAX_NAME_CHARS), "--runtime-generation", _canonical_u64(generation)))


def _bounded(value: Any, maximum: int) -> Any:
    return value[:maximum] if isinstance(value, list) else value


def _normalize_terminal(value: Any, maximum_bytes: int, decode_output: bool = False) -> Any:
    """Normalize display text once while preserving all cursor/outcome metadata."""
    if not isinstance(value, dict):
        return value
    normalized = dict(value)
    if decode_output:
        raw = normalized.pop("data_base64", None)
        if not isinstance(raw, str):
            raise CliError("pohunek_cli_invalid_envelope")
        try:
            output = base64.b64decode(raw.encode("ascii"), validate=True)
        except (UnicodeEncodeError, ValueError, binascii.Error):
            raise CliError("pohunek_cli_invalid_envelope") from None
        if len(output) > maximum_bytes:
            raise CliError("plugin_output_limit_exceeded")
        normalized["text"] = output.decode("utf-8", "replace")
        normalized["utf8_replaced"] = "�" in normalized["text"]
        return normalized
    for key in ("text", "diff", "screen"):
        text = normalized.get(key)
        if isinstance(text, str):
            complete = text.encode("utf-8", "replace")
            encoded = complete[:maximum_bytes]
            normalized[key] = encoded.decode("utf-8", "replace")
            normalized["utf8_replaced"] = bool(normalized.get("utf8_replaced")) or "�" in normalized[key]
            normalized["truncated"] = bool(normalized.get("truncated")) or len(complete) > maximum_bytes
    if isinstance(normalized.get("visible_lines"), list):
        lines = [str(line).replace("\x1b", "") for line in normalized["visible_lines"]]
        text = "\n".join(lines)
        normalized["text"] = text.encode("utf-8", "replace")[:maximum_bytes].decode("utf-8", "replace")
        normalized["utf8_replaced"] = bool(normalized.get("utf8_replaced")) or "�" in normalized["text"]
        normalized["truncated"] = bool(normalized.get("truncated")) or len(text.encode("utf-8", "replace")) > maximum_bytes
    return normalized
