#!/bin/sh
# installed by pohunek
# managed by pohunek; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# POHUNEK_INTEGRATION_ID=claude
# POHUNEK_INTEGRATION_VERSION=2
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
[ -n "${POHUNEK_SOCKET_PATH:-}" ] || exit 0
[ -n "${POHUNEK_SESSION_ID:-}" ] || exit 0
[ -n "${POHUNEK_PROTOCOL_VERSION:-}" ] || exit 0
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

agent = "claude"
ACTION_SESSION = "session"
ACTION_RELEASE = "release"
TIMESTAMP_MS_FACTOR = 1000
SOCKET_TIMEOUT_SECS = 0.5
RESPONSE_BYTES = 4096
MIN_AGENT_PID = 1

action = os.environ.get("POHUNEK_HOOK_ACTION")
session_id = os.environ.get("POHUNEK_SESSION_ID")
socket_path = os.environ.get("POHUNEK_SOCKET_PATH")
protocol_raw = os.environ.get("POHUNEK_PROTOCOL_VERSION")
hook_input_file = os.environ.get("POHUNEK_HOOK_INPUT_FILE")
agent_pid_raw = os.environ.get("POHUNEK_AGENT_PID")

if not session_id or not socket_path or not protocol_raw:
    raise SystemExit(0)
if action not in (ACTION_SESSION, ACTION_RELEASE):
    raise SystemExit(0)

try:
    protocol_version = int(protocol_raw)
except ValueError:
    raise SystemExit(0)

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


def send_request(method, params, suffix):
    request = {
        "v": protocol_version,
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

native_id_params = {
    "session_id": session_id,
    "agent": agent,
    "native_session_id": native_session_id,
}
if transcript_path:
    native_id_params["transcript_path"] = transcript_path

send_request("session.report_native_id", native_id_params, "native")
PY
