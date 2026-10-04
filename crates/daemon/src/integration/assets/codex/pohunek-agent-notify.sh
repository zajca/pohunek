#!/bin/sh
# installed by pohunek
# managed by pohunek; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# POHUNEK_INTEGRATION_ID=codex
# POHUNEK_INTEGRATION_VERSION=10
#
# Codex lifecycle notification hook. Fire-and-forget: any missing handshake
# env, missing python3, invalid input, or socket failure is a silent no-op
# (exit 0) so the hook can never break the agent.

set -eu

action="${1:-}"

case "$action" in
  permission_request|stop) ;;
  *) exit 0 ;;
esac

[ "${POHUNEK_ENV:-}" = "1" ] || exit 0
[ -n "${POHUNEK_WORKER_SOCKET_PATH:-}" ] ||
  { [ -n "${POHUNEK_SOCKET_PATH:-}" ] && [ -n "${POHUNEK_PROTOCOL_VERSION:-}" ]; } || exit 0
command -v python3 >/dev/null 2>&1 || exit 0
agent_pid="$PPID"

# Provider hook payloads can be large and may contain raw prompt/output data.
# Read only the bounded prefix needed for sanitized ids, and never let stdin
# size disrupt the agent process.
MAX_HOOK_INPUT_BYTES=65536

hook_input_file="$(mktemp "${TMPDIR:-/tmp}/pohunek-codex-notify.XXXXXX" 2>/dev/null)" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
head -c "$MAX_HOOK_INPUT_BYTES" >"$hook_input_file" 2>/dev/null || true

POHUNEK_HOOK_INPUT_FILE="$hook_input_file" \
POHUNEK_HOOK_ACTION="$action" \
POHUNEK_AGENT_PID="$agent_pid" \
python3 -I - >/dev/null 2>&1 <<'PY' || exit 0
import json
import os
import socket
import sys
import time

AGENT = "codex"
MAX_HOOK_INPUT_CHARS = 65536
MAX_SESSION_ID_BYTES = 512
SAFE_ID_CHARS = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.:@")
SECRET_MARKERS = (
    "token",
    "secret",
    "password",
    "authorization",
    "cookie",
    "api_key",
    "apikey",
    "bearer",
    "sk-",
    "ghp_",
    "xox",
)

EVENTS = {
    "permission_request": {
        "provider_event": "PermissionRequest",
        "kind": "approval_required",
        "severity": "action_required",
        "title": "Codex approval required",
        "body": "Codex is waiting for approval.",
        "attention": True,
    },
    "stop": {
        "provider_event": "Stop",
        "kind": "turn_completed",
        "severity": "info",
        "title": "Codex turn completed",
        "body": "Codex completed an agent turn.",
        "attention": False,
    },
}


def read_hook_input(path):
    if not path:
        return {}
    try:
        with open(path, encoding="utf-8") as handle:
            content = handle.read(MAX_HOOK_INPUT_CHARS)
        if not content.strip():
            return {}
        parsed = json.loads(content)
        return parsed if isinstance(parsed, dict) else {}
    except Exception:
        return {}


def secret_shaped(value):
    lowered = value.lower()
    if any(marker in lowered for marker in SECRET_MARKERS):
        return True
    compact = value.replace("-", "").replace("_", "")
    if len(compact) >= 40 and all(ch in SAFE_ID_CHARS for ch in compact):
        return True
    return False


def safe_event_id(value):
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        value = str(value)
    if not isinstance(value, str):
        return None
    if not value or len(value) > 96:
        return None
    if any(ch not in SAFE_ID_CHARS for ch in value):
        return None
    if secret_shaped(value):
        return None
    return value


def safe_session_id(value):
    if not isinstance(value, str) or not value:
        return None
    if len(value.encode("utf-8")) > MAX_SESSION_ID_BYTES:
        return None
    if any(ch not in SAFE_ID_CHARS for ch in value):
        return None
    return value


def hook_event_id(hook_input, timestamp_ms):
    for key in ("hook_event_id", "event_id", "id"):
        candidate = safe_event_id(hook_input.get(key))
        if candidate:
            return candidate
    return str(timestamp_ms)


# Darwin `proc_pidinfo(PROC_PIDTBSDINFO)` layout of `struct proc_bsdinfo`: the
# worker compares the report with the same kernel start time, encoded as
# `pbi_start_tvsec * 1_000_000 + pbi_start_tvusec`.
DARWIN_LIBSYSTEM = "/usr/lib/libSystem.B.dylib"
DARWIN_PROC_PIDTBSDINFO = 3
DARWIN_BSDINFO_SIZE = 136
DARWIN_BSDINFO_PID_OFFSET = 12
DARWIN_BSDINFO_START_OFFSET = 120
MICROSECONDS_PER_SECOND = 1_000_000
SOCKET_TIMEOUT_SECS = 0.5
RESPONSE_BYTES = 4096
MIN_AGENT_PID = 1


def darwin_process_start_identity(pid):
    import ctypes
    import struct

    libsystem = ctypes.CDLL(DARWIN_LIBSYSTEM, use_errno=True)
    proc_pidinfo = libsystem.proc_pidinfo
    proc_pidinfo.argtypes = [
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_uint64,
        ctypes.c_void_p,
        ctypes.c_int,
    ]
    proc_pidinfo.restype = ctypes.c_int
    buffer = ctypes.create_string_buffer(DARWIN_BSDINFO_SIZE)
    written = proc_pidinfo(pid, DARWIN_PROC_PIDTBSDINFO, 0, buffer, DARWIN_BSDINFO_SIZE)
    if written != DARWIN_BSDINFO_SIZE:
        return None
    (reported_pid,) = struct.unpack_from("=I", buffer, DARWIN_BSDINFO_PID_OFFSET)
    seconds, microseconds = struct.unpack_from("=QQ", buffer, DARWIN_BSDINFO_START_OFFSET)
    if reported_pid != pid or microseconds >= MICROSECONDS_PER_SECOND:
        return None
    return seconds * MICROSECONDS_PER_SECOND + microseconds


def process_start_identity(pid):
    try:
        if sys.platform == "darwin":
            return darwin_process_start_identity(pid)
        with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
            stat = handle.read()
        fields = stat[stat.rfind(")") + 2:].split()
        return int(fields[19])
    except Exception:
        return None


def send_worker_notification(params, sequence):
    """Delivers through the worker, which attests the caller and forwards to the daemon."""
    worker_socket_path = os.environ.get("POHUNEK_WORKER_SOCKET_PATH")
    worker_instance_id = os.environ.get("POHUNEK_WORKER_INSTANCE_ID") or os.environ.get("POHUNEK_RUNTIME_ID")
    agent_pid_raw = os.environ.get("POHUNEK_AGENT_PID")
    if not worker_socket_path or not worker_instance_id or not agent_pid_raw:
        return False
    try:
        agent_pid = int(agent_pid_raw)
    except ValueError:
        return False
    if agent_pid < MIN_AGENT_PID:
        return False
    start_identity = process_start_identity(agent_pid)
    if start_identity is None:
        return False
    request = {
        "type": "notification_create",
        "runtime_id": worker_instance_id,
        "provider": AGENT,
        "pid": agent_pid,
        "start_identity": start_identity,
        "sequence": sequence,
        "params": params,
    }
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(SOCKET_TIMEOUT_SECS)
        client.connect(worker_socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        response = client.recv(RESPONSE_BYTES)
        client.close()
        return json.loads(response.splitlines()[0]).get("ok") is True
    except Exception:
        return False


def send_request(method, params, request_id):
    request = {
        "v": {"minimum": protocol_version, "maximum": protocol_version},
        "id": request_id,
        "method": method,
        "params": params,
    }
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(SOCKET_TIMEOUT_SECS)
        client.connect(socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        try:
            client.recv(RESPONSE_BYTES)
        except Exception:
            pass
        client.close()
    except Exception:
        pass


session_id = safe_session_id(os.environ.get("POHUNEK_SESSION_ID"))
socket_path = os.environ.get("POHUNEK_SOCKET_PATH")
protocol_raw = os.environ.get("POHUNEK_PROTOCOL_VERSION")
action = os.environ.get("POHUNEK_HOOK_ACTION")
event = EVENTS.get(action)
if event is None or (
    not os.environ.get("POHUNEK_WORKER_SOCKET_PATH") and (not socket_path or not protocol_raw)
):
    raise SystemExit(0)

protocol_version = None
try:
    protocol_version = int(protocol_raw) if protocol_raw else None
except ValueError:
    protocol_version = None

timestamp_ms = int(time.time() * 1000)
hook_input = read_hook_input(os.environ.get("POHUNEK_HOOK_INPUT_FILE"))
event_id = hook_event_id(hook_input, timestamp_ms)
provider_event = event["provider_event"]
source_id = f"hook:{AGENT}:{provider_event}:{event_id}"
metadata = {
    "provider": AGENT,
    "provider_event": provider_event,
    "hook_event_id": event_id,
}
params = {
    "source": {
        "provider": AGENT,
        "provider_event": provider_event,
        "host_local_source_id": source_id,
    },
    "kind": event["kind"],
    "severity": event["severity"],
    "status": "unread",
    "title": event["title"],
    "body": event["body"],
    "metadata": metadata,
    "agent_kind": AGENT,
    "source_id": source_id,
}
if session_id:
    params["session_id"] = session_id
if session_id and event["attention"]:
    params["dedupe_key"] = f"attention:{session_id}"
elif session_id and event["kind"] == "turn_completed":
    params["dedupe_key"] = f"turn:{session_id}"

if send_worker_notification(params, time.monotonic_ns()):
    raise SystemExit(0)
if socket_path and protocol_version is not None:
    send_request("notification.create", params, f"{source_id}:create")
PY
