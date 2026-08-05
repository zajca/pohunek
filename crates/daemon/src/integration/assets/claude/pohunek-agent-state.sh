#!/bin/sh
# installed by pohunek
# managed by pohunek; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# POHUNEK_INTEGRATION_ID=claude
# POHUNEK_INTEGRATION_VERSION=4
#
# SessionStart/SessionEnd hook: report active-agent identity, capture the
# agent's native session id for direct-session resume, and release active-agent
# state on clean session exit. Fire-and-forget: any missing handshake env,
# missing python3, or socket failure is a silent no-op (exit 0) so the hook can
# never break the agent.

set -eu

action="${1:-}"
agent_pid="$PPID"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/pohunek-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session|release) ;;
  *) exit 0 ;;
esac

[ "${POHUNEK_ENV:-}" = "1" ] || exit 0
[ -n "${POHUNEK_WORKER_SOCKET_PATH:-}" ] || [ -n "${POHUNEK_SOCKET_PATH:-}" ] || exit 0
[ -n "${POHUNEK_SESSION_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# `|| exit 0` on the heredoc command itself (NOT a trailing `exit 0`, which
# `set -e` would never reach): an abnormal python exit (OOM, hook timeout kill,
# SIGPIPE) must still leave the SessionStart hook exiting 0 so it never breaks
# the agent.
POHUNEK_HOOK_ACTION="$action" \
POHUNEK_AGENT_PID="$agent_pid" \
POHUNEK_HOOK_INPUT_FILE="$hook_input_file" \
python3 - <<'PY' || exit 0
import json
import os
import socket
import time
from datetime import datetime, timedelta, timezone

agent = "claude"
ACTION_SESSION = "session"
ACTION_RELEASE = "release"
TIMESTAMP_MS_FACTOR = 1000
SOCKET_TIMEOUT_SECS = 0.5
RESPONSE_BYTES = 4096
MIN_AGENT_PID = 1
IDENTITY_TTL_SECS = 30

action = os.environ.get("POHUNEK_HOOK_ACTION")
session_id = os.environ.get("POHUNEK_SESSION_ID")
socket_path = os.environ.get("POHUNEK_SOCKET_PATH")
worker_socket_path = os.environ.get("POHUNEK_WORKER_SOCKET_PATH")
protocol_raw = os.environ.get("POHUNEK_PROTOCOL_VERSION")
runtime_id = os.environ.get("POHUNEK_RUNTIME_ID")
hook_input_file = os.environ.get("POHUNEK_HOOK_INPUT_FILE")
agent_pid_raw = os.environ.get("POHUNEK_AGENT_PID")

if not session_id or (not worker_socket_path and (not socket_path or not protocol_raw)):
    raise SystemExit(0)
if action not in (ACTION_SESSION, ACTION_RELEASE):
    raise SystemExit(0)

protocol_version = None
if socket_path and protocol_raw:
    try:
        protocol_version = int(protocol_raw)
    except ValueError:
        protocol_version = None

try:
    parsed_agent_pid = int(agent_pid_raw) if agent_pid_raw else None
except ValueError:
    parsed_agent_pid = None
agent_pid = parsed_agent_pid if parsed_agent_pid and parsed_agent_pid >= MIN_AGENT_PID else None

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}

native = hook_input.get("session_id")
native_session_id = native if isinstance(native, str) and native else None
transcript = hook_input.get("transcript_path")
transcript_path = transcript if isinstance(transcript, str) and transcript else None

timestamp_ms = int(time.time() * TIMESTAMP_MS_FACTOR)


def process_start_identity(pid):
    try:
        with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
            stat = handle.read()
        fields = stat[stat.rfind(")") + 2:].split()
        return int(fields[19])
    except Exception:
        return None


def send_worker_hook(request_type):
    if not worker_socket_path or not runtime_id or agent_pid is None:
        return False
    start_identity = process_start_identity(agent_pid)
    if start_identity is None:
        return False
    request = {
        "type": request_type,
        "runtime_id": runtime_id,
        "provider": agent,
        "pid": agent_pid,
        "start_identity": start_identity,
        "sequence": timestamp_ms,
    }
    if request_type == "identity_report":
        reference_kind = os.environ.get("POHUNEK_NATIVE_REFERENCE_KIND")
        native_reference = transcript_path if reference_kind == "path" else native_session_id
        request.update({
            "expires_at": (
                datetime.now(timezone.utc) + timedelta(seconds=IDENTITY_TTL_SECS)
            ).isoformat().replace("+00:00", "Z"),
            "reference_kind": reference_kind,
            "native_reference": native_reference if reference_kind in ("id", "path") else None,
        })
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(SOCKET_TIMEOUT_SECS)
        client.connect(worker_socket_path)
        client.sendall((json.dumps(request) + "\n").encode())
        response = client.recv(RESPONSE_BYTES)
        client.close()
        result = json.loads(response.splitlines()[0])
        return result.get("ok") is True
    except Exception:
        return False


def send_request(method, params, suffix):
    request = {
        "v": {"minimum": protocol_version, "maximum": protocol_version},
        "id": f"hook:{agent}:{timestamp_ms}:{suffix}",
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


if action == ACTION_RELEASE:
    if worker_socket_path and send_worker_hook("identity_release"):
        raise SystemExit(0)
    if not socket_path or protocol_version is None:
        raise SystemExit(0)
    release_agent_params = {
        "session_id": session_id,
        "source": f"pohunek:{agent}",
        "agent": agent,
        "seq": timestamp_ms,
    }
    send_request("session.release_agent", release_agent_params, "release")
    raise SystemExit(0)

if not native_session_id:
    raise SystemExit(0)

if worker_socket_path and send_worker_hook("identity_report"):
    raise SystemExit(0)

if not socket_path or protocol_version is None:
    raise SystemExit(0)

report_agent_params = {
    "session_id": session_id,
    "source": f"pohunek:{agent}",
    "agent": agent,
    "seq": timestamp_ms,
    "agent_session_id": native_session_id,
}
if agent_pid is not None:
    report_agent_params["pid"] = agent_pid
if transcript_path:
    report_agent_params["agent_session_path"] = transcript_path

send_request("session.report_agent", report_agent_params, "agent")

if runtime_id and agent_pid is not None:
    start_identity = process_start_identity(agent_pid)
    if start_identity is not None:
        native_id_params = {
            "session_id": session_id,
            "runtime_id": runtime_id,
            "agent": agent,
            "pid": agent_pid,
            "pid_start_identity": str(start_identity),
            "sequence": str(timestamp_ms),
            "expires_at": (
                datetime.now(timezone.utc) + timedelta(seconds=IDENTITY_TTL_SECS)
            ).isoformat().replace("+00:00", "Z"),
            "native_session_id": native_session_id,
        }
        if transcript_path:
            native_id_params["transcript_path"] = transcript_path
        send_request("session.report_native_id", native_id_params, "native")
PY
