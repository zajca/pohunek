#!/bin/sh
# installed by pohunek
# managed by pohunek; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# POHUNEK_INTEGRATION_ID=claude
# POHUNEK_INTEGRATION_VERSION=2
#
# Claude notification hook. Fire-and-forget: any missing handshake env, missing
# python3, invalid input, or socket failure is a silent no-op (exit 0) so the
# hook can never break the agent.

set -eu

action="${1:-}"
matcher="${2:-}"

case "$action" in
  notification|stop|stop_failure) ;;
  *) exit 0 ;;
esac

[ "${POHUNEK_ENV:-}" = "1" ] || exit 0
[ -n "${POHUNEK_SOCKET_PATH:-}" ] || exit 0
[ -n "${POHUNEK_PROTOCOL_VERSION:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# Provider hook payloads can be large and may contain raw prompt/output data.
# Read only the bounded prefix needed for sanitized ids, and never let stdin
# size disrupt the agent process.
MAX_HOOK_INPUT_BYTES=65536

hook_input_file="$(mktemp "${TMPDIR:-/tmp}/pohunek-claude-notify.XXXXXX" 2>/dev/null)" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
head -c "$MAX_HOOK_INPUT_BYTES" >"$hook_input_file" 2>/dev/null || true

POHUNEK_HOOK_INPUT_FILE="$hook_input_file" \
POHUNEK_HOOK_ACTION="$action" \
POHUNEK_HOOK_MATCHER="$matcher" \
python3 - >/dev/null 2>&1 <<'PY' || exit 0
import json
import os
import socket
import time

AGENT = "claude"
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

NOTIFICATION_MATCHERS = {
    "permission_prompt": {
        "kind": "approval_required",
        "severity": "action_required",
        "title": "Claude approval required",
        "body": "Claude is waiting for approval.",
        "attention": True,
    },
    "elicitation_dialog": {
        "kind": "approval_required",
        "severity": "action_required",
        "title": "Claude response required",
        "body": "Claude is waiting for approval.",
        "attention": True,
    },
    "idle_prompt": {
        "kind": "agent_blocked",
        "severity": "warning",
        "title": "Claude needs input",
        "body": "Claude is waiting for input.",
        "attention": True,
    },
    "auth_success": {
        "kind": "system",
        "severity": "success",
        "title": "Claude authentication succeeded",
        "body": "Claude reported successful authentication.",
        "attention": False,
    },
    "elicitation_complete": {
        "kind": "system",
        "severity": "info",
        "title": "Claude elicitation completed",
        "body": "Claude completed an elicitation flow.",
        "attention": False,
    },
    "elicitation_response": {
        "kind": "system",
        "severity": "info",
        "title": "Claude elicitation response",
        "body": "Claude received an elicitation response.",
        "attention": False,
    },
}

EVENTS = {
    "stop": {
        "provider_event": "Stop",
        "kind": "turn_completed",
        "severity": "info",
        "title": "Claude turn completed",
        "body": "Claude completed an agent turn.",
        "attention": False,
        "matcher": None,
    },
    "stop_failure": {
        "provider_event": "StopFailure",
        "kind": "error",
        "severity": "error",
        "title": "Claude stop hook failed",
        "body": "Claude reported a stop hook failure.",
        "attention": False,
        "matcher": None,
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


def safe_matcher(value):
    return value if value in NOTIFICATION_MATCHERS else None


def hook_event_id(hook_input, timestamp_ms):
    for key in ("hook_event_id", "event_id", "id"):
        candidate = safe_event_id(hook_input.get(key))
        if candidate:
            return candidate
    return str(timestamp_ms)


def send_request(method, params, request_id):
    request = {
        "v": protocol_version,
        "id": request_id,
        "method": method,
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


session_id = safe_session_id(os.environ.get("POHUNEK_SESSION_ID"))
socket_path = os.environ.get("POHUNEK_SOCKET_PATH")
protocol_raw = os.environ.get("POHUNEK_PROTOCOL_VERSION")
action = os.environ.get("POHUNEK_HOOK_ACTION")
hook_input = read_hook_input(os.environ.get("POHUNEK_HOOK_INPUT_FILE"))
if not socket_path or not protocol_raw:
    raise SystemExit(0)

try:
    protocol_version = int(protocol_raw)
except ValueError:
    raise SystemExit(0)

event = None
matcher = None
if action == "notification":
    matcher = safe_matcher(os.environ.get("POHUNEK_HOOK_MATCHER"))
    if matcher is None:
        matcher = safe_matcher(hook_input.get("matcher"))
    if matcher is None:
        matcher = safe_matcher(hook_input.get("type"))
    if matcher is None:
        raise SystemExit(0)
    event = dict(NOTIFICATION_MATCHERS[matcher])
    event["provider_event"] = f"Notification.{matcher}"
    event["matcher"] = matcher
else:
    event = EVENTS.get(action)
    if event is None:
        raise SystemExit(0)

timestamp_ms = int(time.time() * 1000)
event_id = hook_event_id(hook_input, timestamp_ms)
provider_event = event["provider_event"]
source_id = f"hook:{AGENT}:{provider_event}:{event_id}"
metadata = {
    "provider": AGENT,
    "provider_event": provider_event,
    "hook_event_id": event_id,
}
if event["matcher"]:
    metadata["matcher"] = event["matcher"]
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

send_request("notification.create", params, f"{source_id}:create")
PY
