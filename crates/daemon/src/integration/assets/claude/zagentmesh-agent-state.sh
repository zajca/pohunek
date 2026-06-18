#!/bin/sh
# installed by zagentmesh
# managed by zagentmesh; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# ZAGENTMESH_INTEGRATION_ID=claude
# ZAGENTMESH_INTEGRATION_VERSION=1
#
# SessionStart hook: capture the agent's native session id for resume. This is
# session-id capture ONLY; it never reports live state. Fire-and-forget: any
# missing handshake env, missing python3, or socket failure is a silent no-op
# (exit 0) so the hook can never break the agent.

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/zagentmesh-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  session) ;;
  *) exit 0 ;;
esac

[ "${ZAGENTMESH_ENV:-}" = "1" ] || exit 0
[ -n "${ZAGENTMESH_SOCKET_PATH:-}" ] || exit 0
[ -n "${ZAGENTMESH_SESSION_ID:-}" ] || exit 0
[ -n "${ZAGENTMESH_PROTOCOL_VERSION:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# `|| exit 0` on the heredoc command itself (NOT a trailing `exit 0`, which
# `set -e` would never reach): an abnormal python exit (OOM, hook timeout kill,
# SIGPIPE) must still leave the SessionStart hook exiting 0 so it never breaks
# the agent.
ZAGENTMESH_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY' || exit 0
import json
import os
import socket
import time

agent = "claude"
session_id = os.environ.get("ZAGENTMESH_SESSION_ID")
socket_path = os.environ.get("ZAGENTMESH_SOCKET_PATH")
protocol_raw = os.environ.get("ZAGENTMESH_PROTOCOL_VERSION")
hook_input_file = os.environ.get("ZAGENTMESH_HOOK_INPUT_FILE")

if not session_id or not socket_path or not protocol_raw:
    raise SystemExit(0)

try:
    protocol_version = int(protocol_raw)
except ValueError:
    raise SystemExit(0)

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

if not native_session_id:
    raise SystemExit(0)

params = {
    "session_id": session_id,
    "agent": agent,
    "native_session_id": native_session_id,
}
if transcript_path:
    params["transcript_path"] = transcript_path

request = {
    "v": protocol_version,
    "id": f"hook:{agent}:{int(time.time() * 1000)}",
    "method": "session.report_native_id",
    "params": params,
}

try:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(0.5)
    client.connect(socket_path)
    client.sendall((json.dumps(request) + "\n").encode())
    try:
        client.recv(4096)
    except Exception:
        pass
    client.close()
except Exception:
    pass
PY
