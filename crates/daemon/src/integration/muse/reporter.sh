#!/bin/sh
# installed by pohunek
# managed by pohunek; the daemon rewrites this file for every Muse launch.
# POHUNEK_INTEGRATION_ID=muse
# POHUNEK_INTEGRATION_VERSION=1
#
# Muse hook reporter: reports the root session identity and the provider-managed
# subagent lifecycle to the session worker. Stateless: it writes no file but the
# hook trace. Fire-and-forget: every failure exits 0 without output so the hook
# can never break the agent. A run that does not deliver its report appends one
# bounded line per reason to the hook trace named by POHUNEK_HOOK_TRACE_PATH.
#
# UNVERIFIED: the hook payload fields below come from the Muse documentation,
# the MIT Muse Code SDK schema and its changelog (release 1.4.13), not from a
# capture of a real Muse run.

set -eu

# Muse clears the hook environment; the launch lists PATH among the variables it
# keeps, and the fallback below covers a Muse that passes none. A host whose
# python3 is elsewhere (or a stub that is not a working interpreter) makes the
# hook exit without a report.
[ -n "${PATH:-}" ] || PATH=/usr/local/bin:/usr/bin:/bin
export PATH

action="${1:-}"
case "$action" in
  session|subagent-start|subagent-stop) ;;
  *) exit 0 ;;
esac

[ "${POHUNEK_ENV:-}" = "1" ] || exit 0
[ -n "${POHUNEK_SESSION_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0
agent_pid="$PPID"

POHUNEK_HOOK_ACTION="$action" \
POHUNEK_AGENT_PID="$agent_pid" \
python3 -I - 3<&0 <<'PY' || exit 0
import atexit
import errno
import fcntl
import json
import os
import socket
import stat
import sys
import time
from datetime import datetime, timedelta, timezone

agent = "muse"
ACTION_SESSION = "session"
ACTION_SUBAGENT_START = "subagent-start"
ACTION_SUBAGENT_STOP = "subagent-stop"
ACTIONS = (ACTION_SESSION, ACTION_SUBAGENT_START, ACTION_SUBAGENT_STOP)
TIMESTAMP_MS_FACTOR = 1000
SOCKET_TIMEOUT_SECS = 0.5
RESPONSE_BYTES = 4096
MIN_AGENT_PID = 1
IDENTITY_TTL_SECS = 30
# Bounds provider JSON before decoding; oversized payloads are rejected whole.
MAX_HOOK_INPUT_BYTES = 65536
# Bounds one trace line in bytes, enough for every field at its longest fixed
# code and a session id; a longer line is dropped. Lines of concurrent hooks
# stay apart because each is one write under the exclusive trace lock.
MAX_TRACE_LINE_BYTES = 512
# The trace is appended without following a symlink and created owner-private.
# O_NONBLOCK makes a FIFO at the path fail the open instead of blocking the hook.
TRACE_OPEN_FLAGS = (
    os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
)
TRACE_FILE_MODE = 0o600
# Group and other permission bits; a trace carrying any of them is not written.
TRACE_SHARED_MODE_BITS = 0o077
# The single rotated copy; the trace and this copy together hold at most twice
# the configured bound.
TRACE_ROTATED_SUFFIX = ".1"
# One open that finds the trace already rotated by another hook, one open
# after this hook's own rotation, and one spare for a rotation racing both.
TRACE_OPEN_ATTEMPTS = 3
# Digits accepted in POHUNEK_HOOK_TRACE_MAX_BYTES. Fifteen digits exceed any
# real trace bound and stay far below the length at which int() raises.
MAX_TRACE_BYTES_DIGITS = 15
# Non-blocking lock attempts before a contended trace is given up for this run.
# A hook holds the lock for one fstat, rename and write, so a free lock shows
# up within a few retries, while a holder that never lets go costs a run at most
# (TRACE_LOCK_ATTEMPTS - 1) * TRACE_LOCK_RETRY_SECONDS, 20 ms.
TRACE_LOCK_ATTEMPTS = 5
TRACE_LOCK_RETRY_SECONDS = 0.005

session_id = os.environ.get("POHUNEK_SESSION_ID")
action = os.environ.get("POHUNEK_HOOK_ACTION")
worker_socket_path = os.environ.get("POHUNEK_WORKER_SOCKET_PATH")
worker_instance_id = os.environ.get("POHUNEK_WORKER_INSTANCE_ID")
agent_pid_raw = os.environ.get("POHUNEK_AGENT_PID")
trace_path = os.environ.get("POHUNEK_HOOK_TRACE_PATH")
trace_max_raw = os.environ.get("POHUNEK_HOOK_TRACE_MAX_BYTES")
reference_kind = os.environ.get("POHUNEK_NATIVE_REFERENCE_KIND")
# Muse sets the root session id for every hook command, whatever thread fires it.
muse_session_id = os.environ.get("MUSE_SESSION_ID")


def positive_int(raw):
    # ASCII digits without a leading zero, the form the worker writes; any
    # other value, including an overlong one, disables the trace.
    if (
        raw
        and len(raw) <= MAX_TRACE_BYTES_DIGITS
        and raw.isascii()
        and raw.isdigit()
        and raw[0] != "0"
    ):
        return int(raw)
    return None


trace_max_bytes = positive_int(trace_max_raw)
trace_enabled = bool(trace_path) and os.path.isabs(trace_path) and trace_max_bytes is not None
trace_lines = []


def trace(reason, detail=None):
    # Queues one line naming why a report was not delivered. A line holds only
    # fixed codes, the pohunek session id and the time; the queue is written
    # at exit, so tracing never delays an exchange.
    if not trace_enabled:
        return
    entry = {
        "ts": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
        "session": session_id,
        "provider": agent,
        "action": action if action in ACTIONS else None,
        "reason": reason,
    }
    if detail:
        entry["detail"] = detail
    line = (json.dumps(entry, separators=(",", ":")) + "\n").encode("ascii")
    if len(line) <= MAX_TRACE_LINE_BYTES and len(line) <= trace_max_bytes:
        trace_lines.append(line)


def errno_name(error):
    return errno.errorcode.get(error.errno) if error.errno else None


def lock_trace(descriptor):
    # Takes the exclusive trace lock without ever blocking the hook on it.
    for attempt in range(TRACE_LOCK_ATTEMPTS):
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return True
        except BlockingIOError:
            if attempt + 1 < TRACE_LOCK_ATTEMPTS:
                time.sleep(TRACE_LOCK_RETRY_SECONDS)
    return False


def append_trace_line(line):
    # Writes `line` with one O_APPEND write under an exclusive lock, so the size
    # check, the single rotation and the write of concurrent hooks never mix.
    # Returns False when the lock stays contended or the write came up short,
    # and the line is dropped.
    rotated = False
    for _ in range(TRACE_OPEN_ATTEMPTS):
        descriptor = os.open(trace_path, TRACE_OPEN_FLAGS, TRACE_FILE_MODE)
        try:
            if not lock_trace(descriptor):
                return False
            opened = os.fstat(descriptor)
            if (
                not stat.S_ISREG(opened.st_mode)
                or opened.st_uid != os.getuid()
                or opened.st_nlink != 1
                or opened.st_mode & TRACE_SHARED_MODE_BITS
            ):
                return True
            try:
                current = os.lstat(trace_path)
            except FileNotFoundError:
                continue
            if (current.st_dev, current.st_ino) != (opened.st_dev, opened.st_ino):
                # Another hook rotated the trace while this one waited for the lock.
                continue
            if opened.st_size + len(line) <= trace_max_bytes:
                if os.write(descriptor, line) != len(line):
                    # A short write (full disk, quota, file size limit) leaves
                    # a line without its newline; it is cut off again under
                    # the lock so the next line never fuses with it.
                    os.ftruncate(descriptor, opened.st_size)
                    return False
                return True
            if rotated:
                return True
            os.replace(trace_path, trace_path + TRACE_ROTATED_SUFFIX)
            rotated = True
        finally:
            os.close(descriptor)
    return True


def flush_trace():
    # Any trace failure drops the remaining lines: the trace never changes the
    # exit or output, and a contended lock costs a run its retry budget once.
    for line in trace_lines:
        try:
            if not append_trace_line(line):
                return
        except OSError:
            return


if not session_id:
    raise SystemExit(0)
atexit.register(flush_trace)

if not worker_socket_path:
    trace("worker_socket_missing", "unset")
    raise SystemExit(0)
if action not in ACTIONS:
    trace("action_invalid")
    raise SystemExit(0)

try:
    parsed_agent_pid = int(agent_pid_raw) if agent_pid_raw else None
except ValueError:
    parsed_agent_pid = None
agent_pid = parsed_agent_pid if parsed_agent_pid and parsed_agent_pid >= MIN_AGENT_PID else None


def worker_route_problem():
    # The trace reason that keeps a request from the worker, or None.
    if not worker_socket_path:
        return ("worker_socket_missing", "unset")
    if not worker_instance_id:
        return ("worker_instance_missing", None)
    if agent_pid is None:
        return ("agent_pid_invalid", None)
    return None


# Thread ids Muse gives its background reminder children end with this suffix;
# they are not conversations or subagents.
REMINDER_SUFFIX = "-reminder"
# Session sources a SessionStart payload names. Each one binds the reported
# session id as the conversation of this pohunek session, so a clear, a resume
# and a fork re-bind; any other value is refused.
KNOWN_SOURCES = ("startup", "resume", "clear", "compact", "fork")


def read_hook_input():
    # The payload object and None, or {} and the trace reason and detail that
    # voided the input.
    try:
        with os.fdopen(3, "rb") as handle:
            content = handle.read(MAX_HOOK_INPUT_BYTES + 1)
    except OSError as error:
        return {}, ("input_invalid", errno_name(error))
    if len(content) > MAX_HOOK_INPUT_BYTES:
        return {}, ("input_too_large", None)
    if not content.strip():
        return {}, ("input_invalid", "empty")
    try:
        parsed = json.loads(content)
    except RecursionError:
        return {}, ("input_invalid", "too_deep")
    except UnicodeDecodeError:
        return {}, ("input_invalid", "not_utf8")
    except json.JSONDecodeError:
        return {}, ("input_invalid", "not_json")
    if not isinstance(parsed, dict):
        return {}, ("input_invalid", "not_object")
    return parsed, None


hook_input, input_problem = read_hook_input()


def text_field(name):
    value = hook_input.get(name)
    return value if isinstance(value, str) and value else None


def missing_input(detail):
    # The input's own problem when it was voided, else the missing field.
    return input_problem if input_problem is not None else ("input_invalid", detail)


timestamp_ms = int(time.time() * TIMESTAMP_MS_FACTOR)
# Every hook event is its own short-lived interpreter, and the worker orders the
# reports of one agent by this value. `time.monotonic_ns()` is only defined
# within one process, so the system-wide monotonic clock is read instead.
sequence = time.clock_gettime_ns(time.CLOCK_MONOTONIC)


# Darwin `proc_pidinfo(PROC_PIDTBSDINFO)` layout of `struct proc_bsdinfo`: the
# worker compares the report with the same kernel start time, encoded as
# `pbi_start_tvsec * 1_000_000 + pbi_start_tvusec`.
DARWIN_LIBSYSTEM = "/usr/lib/libSystem.B.dylib"
DARWIN_PROC_PIDTBSDINFO = 3
DARWIN_BSDINFO_SIZE = 136
DARWIN_BSDINFO_PID_OFFSET = 12
DARWIN_BSDINFO_START_OFFSET = 120
MICROSECONDS_PER_SECOND = 1_000_000


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


start_identity_read = False
start_identity_value = None


def agent_start_identity():
    # The agent's start identity, read once per run so a missing identity is
    # traced once whichever report needed it first.
    global start_identity_read, start_identity_value
    if not start_identity_read:
        start_identity_value = process_start_identity(agent_pid)
        start_identity_read = True
        if start_identity_value is None:
            trace("agent_identity_unavailable")
    return start_identity_value


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


def exchange(path, request, route):
    # Sends one request line and returns the answer object, or traces why there
    # is none with a reason prefixed by `route`.
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    except OSError as error:
        trace(route + "_unreachable", errno_name(error))
        return None
    try:
        client.settimeout(SOCKET_TIMEOUT_SECS)
        try:
            client.connect(path)
        except socket.timeout:
            trace(route + "_timeout")
            return None
        except FileNotFoundError as error:
            trace(route + "_socket_missing", errno_name(error))
            return None
        except OSError as error:
            trace(route + "_unreachable", errno_name(error))
            return None
        try:
            client.sendall((json.dumps(request) + "\n").encode())
            response = client.recv(RESPONSE_BYTES)
        except socket.timeout:
            trace(route + "_timeout")
            return None
        except OSError as error:
            trace(route + "_closed", errno_name(error))
            return None
    finally:
        client.close()
    lines = response.splitlines()
    if not lines:
        trace(route + "_closed", "empty")
        return None
    try:
        answer = json.loads(lines[0])
    except (ValueError, RecursionError):
        answer = None
    if not isinstance(answer, dict):
        trace(route + "_closed", "malformed")
        return None
    return answer


def send_worker(request):
    answer = exchange(worker_socket_path, request, "worker")
    if answer is None:
        return
    if answer.get("ok") is not True:
        trace("worker_refused")


def base_request(request_type):
    # None when the worker route or the agent identity is unknown.
    problem = worker_route_problem()
    if problem is not None:
        trace(*problem)
        return None
    start_identity = agent_start_identity()
    if start_identity is None:
        return None
    return {
        "type": request_type,
        "runtime_id": worker_instance_id,
        "provider": agent,
        "pid": agent_pid,
        "start_identity": start_identity,
        "sequence": sequence,
    }


def report_identity():
    if muse_session_id is None:
        trace("muse_session_missing")
        return
    native = text_field("session_id")
    if native is None:
        trace(*missing_input("session_id_missing"))
        return
    if native.endswith(REMINDER_SUFFIX):
        trace("reminder_thread")
        return
    if native != muse_session_id:
        trace("thread_not_root")
        return
    source = hook_input.get("source")
    if source is not None and source not in KNOWN_SOURCES:
        trace("source_unknown")
        return
    request = base_request("identity_report")
    if request is None:
        return
    request["expires_at"] = (
        datetime.now(timezone.utc) + timedelta(seconds=IDENTITY_TTL_SECS)
    ).isoformat().replace("+00:00", "Z")
    request["reference_kind"] = reference_kind if reference_kind == "id" else None
    request["native_reference"] = native if reference_kind == "id" else None
    send_worker(request)


def report_subagent(request_type):
    if muse_session_id is None:
        trace("muse_session_missing")
        return
    subagent_id = text_field("subagent_id")
    if subagent_id is None:
        trace(*missing_input("subagent_id_missing"))
        return
    if subagent_id.endswith(REMINDER_SUFFIX):
        trace("reminder_thread")
        return
    request = base_request(request_type)
    if request is None:
        return
    request["subagent_id"] = subagent_id
    if request_type == "subagent_start":
        # Muse names no role on SubagentStart.
        request["agent_type"] = None
        request["parent_id"] = muse_session_id
    else:
        request["outcome"] = "completed"
    send_worker(request)


if action == ACTION_SESSION:
    report_identity()
elif action == ACTION_SUBAGENT_START:
    report_subagent("subagent_start")
else:
    report_subagent("subagent_stop")
PY
