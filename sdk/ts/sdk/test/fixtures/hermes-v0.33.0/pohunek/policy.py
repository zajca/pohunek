"""Fail-closed parser for the installer-rendered Pohunek plugin policy."""

from __future__ import annotations

import json
import os
import stat
from dataclasses import dataclass
import ipaddress
import re
from typing import Any


# Keep the sentinel out of the installer replacement search: only __init__.py
# contains the exact token that is substituted with the selected absolute path.
POLICY_PATH_TOKEN = "__POHUNEK_" + "POLICY_PATH__"
POLICY_SCHEMA_VERSION = 1
ACCESS_MODES = frozenset(("read_only", "manage", "full"))
MAX_TIMEOUT_MS = 60_000
MAX_OUTPUT_BYTES = 1_048_576
MAX_SCREEN_BYTES = 262_144
MAX_CONCURRENCY = 8
MIN_TIMEOUT_MS = 1
MIN_OUTPUT_BYTES = 1
MIN_SCREEN_BYTES = 1
MIN_CONCURRENCY = 1
_HOST_NAME = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,62})(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,62}))*$")


class PolicyError(ValueError):
    """A safe explanation for a rejected plugin policy."""


@dataclass(frozen=True)
class Policy:
    """Validated, immutable delegated-tool settings."""

    pohunek_cli: str
    protocol_min: int
    protocol_max: int
    access_mode: str
    allowed_hosts: frozenset[str]
    tool_timeout_ms: int
    request_timeout_ms: int
    max_output_bytes: int
    max_screen_bytes: int
    max_concurrency: int

    def allows_manage(self) -> bool:
        return self.access_mode in {"manage", "full"}

    def allows_full(self) -> bool:
        return self.access_mode == "full"


def load_policy(rendered_path: str) -> Policy:
    """Load exactly the installer-selected absolute policy file."""
    if rendered_path == POLICY_PATH_TOKEN:
        raise PolicyError("policy path was not materialized")
    if not isinstance(rendered_path, str) or not os.path.isabs(rendered_path):
        raise PolicyError("policy path must be absolute")
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(rendered_path, flags)
    except OSError as error:
        raise PolicyError("policy file is unavailable") from error
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) & 0o077:
            raise PolicyError("policy file is not owner-private")
        with os.fdopen(descriptor, "r", encoding="utf-8", closefd=True) as handle:
            descriptor = -1
            raw = json.load(handle)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PolicyError("policy file is invalid") from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    return _validate(raw)


def _integer(raw: dict[str, Any], name: str, minimum: int, maximum: int) -> int:
    value = raw.get(name)
    if isinstance(value, bool) or not isinstance(value, int):
        raise PolicyError(f"policy field {name} must be an integer")
    if not minimum <= value <= maximum:
        raise PolicyError(f"policy field {name} is out of range")
    return value


def _validate(raw: Any) -> Policy:
    if not isinstance(raw, dict):
        raise PolicyError("policy must be an object")
    required = {
        "schema_version", "pohunek_cli", "protocol_min", "protocol_max", "access_mode",
        "allowed_hosts", "tool_timeout_ms", "request_timeout_ms", "max_output_bytes",
        "max_screen_bytes", "max_concurrency",
    }
    if set(raw) != required:
        raise PolicyError("policy fields do not match the supported schema")
    if raw["schema_version"] != POLICY_SCHEMA_VERSION:
        raise PolicyError("unsupported policy schema")
    cli = raw["pohunek_cli"]
    if not isinstance(cli, str) or not cli or not os.path.isabs(cli):
        raise PolicyError("pohunek_cli must be an absolute path")
    if not os.path.isfile(cli) or not os.access(cli, os.X_OK):
        raise PolicyError("pohunek_cli is unavailable")
    protocol_min = _integer(raw, "protocol_min", 1, 2**31 - 1)
    protocol_max = _integer(raw, "protocol_max", 1, 2**31 - 1)
    if protocol_min > protocol_max:
        raise PolicyError("protocol range is invalid")
    access_mode = raw["access_mode"]
    if access_mode not in ACCESS_MODES:
        raise PolicyError("access_mode is invalid")
    hosts = raw["allowed_hosts"]
    if not isinstance(hosts, list) or not hosts or any(not valid_host(host, allow_wildcard=True) for host in hosts):
        raise PolicyError("allowed_hosts must be a nonempty string list")
    if len(set(hosts)) != len(hosts):
        raise PolicyError("allowed_hosts must not contain duplicates")
    tool_timeout_ms = _integer(raw, "tool_timeout_ms", MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)
    request_timeout_ms = _integer(
        raw, "request_timeout_ms", MIN_TIMEOUT_MS, MAX_TIMEOUT_MS
    )
    if request_timeout_ms >= tool_timeout_ms:
        raise PolicyError("request_timeout_ms must be less than tool_timeout_ms")
    return Policy(
        pohunek_cli=cli,
        protocol_min=protocol_min,
        protocol_max=protocol_max,
        access_mode=access_mode,
        allowed_hosts=frozenset(hosts),
        tool_timeout_ms=tool_timeout_ms,
        request_timeout_ms=request_timeout_ms,
        max_output_bytes=_integer(raw, "max_output_bytes", MIN_OUTPUT_BYTES, MAX_OUTPUT_BYTES),
        max_screen_bytes=_integer(raw, "max_screen_bytes", MIN_SCREEN_BYTES, MAX_SCREEN_BYTES),
        max_concurrency=_integer(raw, "max_concurrency", MIN_CONCURRENCY, MAX_CONCURRENCY),
    )


def valid_host(value: Any, *, allow_wildcard: bool = False) -> bool:
    if not isinstance(value, str) or not value or len(value) > 253 or any(character.isspace() or ord(character) < 32 for character in value):
        return False
    if value == "local" or (allow_wildcard and value == "*"):
        return True
    try:
        ipaddress.ip_address(value)
        return False
    except ValueError:
        return _HOST_NAME.fullmatch(value) is not None
