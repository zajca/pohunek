#!/bin/sh
# installed by pohunek
# managed by pohunek; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# POHUNEK_INTEGRATION_ID=claude
# POHUNEK_INTEGRATION_VERSION=16
#
# Session and subagent lifecycle hook: report active-agent identity, capture the
# agent's native session id for direct-session resume, release active-agent
# state on clean session exit, journal sanitized child state, report the
# main agent's background task list at the end of each turn, and report that a
# submitted prompt was accepted. The end of a turn also repeats the identity
# report, so a SessionStart hook the agent killed before it reported is
# recovered at the next turn.
# Fire-and-forget: every failure exits 0 without output so the hook can never
# break the agent. Inside a pohunek session with python3 available, a run that
# does not deliver its report appends one bounded line per reason to the hook
# trace named by POHUNEK_HOOK_TRACE_PATH, when the worker provides one. The
# SessionStart run also leaves `hook_started` as soon as the interpreter runs
# its script and `hook_finished` when it ends, so a hook the agent killed after
# that shows as started without a finish. A hook the agent killed while the
# interpreter itself was still starting leaves a start marker beside the trace,
# which the shell creates before it starts the interpreter.

set -eu

action="${1:-}"
case "$action" in
  session|release|subagent-start|subagent-stop|stop|prompt-submit) ;;
  *) exit 0 ;;
esac

[ "${POHUNEK_ENV:-}" = "1" ] || exit 0
[ -n "${POHUNEK_SESSION_ID:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0
# The SessionStart run records that it began before the interpreter starts,
# because a slow first `python3` start (the Xcode shim) can outlast the hook
# timeout, and a run killed there never reaches the trace code. A POSIX shell
# cannot append to the trace without following a symlink, but a redirection
# under `set -C` creates a new file exclusively and so never follows one: the
# marker is a new owner-private file named after the trace, holding the pohunek
# session id. The interpreter removes it once it runs, so a marker left behind
# names a run killed before that. At most 16 markers exist at once; the
# interpreter removes markers older than an hour.
start_marker=""
if [ "$action" = "session" ]; then
  case "${POHUNEK_HOOK_TRACE_PATH:-}" in
    /*)
      case "$POHUNEK_SESSION_ID" in
        *[!A-Za-z0-9._-]*) ;;
        *)
          marker_base="$POHUNEK_HOOK_TRACE_PATH.start."
          marker_count=0
          for existing in "$marker_base"*; do
            if [ -e "$existing" ] || [ -L "$existing" ]; then
              marker_count=$((marker_count + 1))
            fi
          done
          if [ "$marker_count" -lt 16 ] &&
            (umask 077 && set -C && printf '%s\n' "$POHUNEK_SESSION_ID" >"$marker_base$$") 2>/dev/null; then
            start_marker="$marker_base$$"
          fi
          ;;
      esac
      ;;
  esac
fi
# BEGIN agent-pid
# Claude runs a hook command as `/bin/sh -c "sh '<this script>' <action>"`. A
# shell that does not exec its last command (dash) stays between the agent and
# this script, so the agent is the parent of that wrapper shell, not `$PPID`.
# Only a parent whose full command line contains exactly that hook invocation,
# with the script path quoted the way the installer quotes it, is skipped.
agent_pid="$PPID"
parent_args="$(ps -ww -o args= -p "$agent_pid" 2>/dev/null || true)"
quoted_self="$(printf '%s' "$0" | sed "s/'/'\"'\"'/g")"
hook_invocation=" -c sh '$quoted_self' "
case "$parent_args" in
  *"$hook_invocation"*)
    outer_pid="$(ps -o ppid= -p "$agent_pid" 2>/dev/null || true)"
    outer_pid="${outer_pid#"${outer_pid%%[![:space:]]*}"}"
    outer_pid="${outer_pid%%[[:space:]]*}"
    case "$outer_pid" in
      ''|*[!0-9]*|0|1) ;;
      *) agent_pid="$outer_pid" ;;
    esac
    ;;
esac
# END agent-pid

# `|| exit 0` on the heredoc command itself (NOT a trailing `exit 0`, which
# `set -e` would never reach): an abnormal python exit (OOM, hook timeout kill,
# SIGPIPE) must still leave the SessionStart hook exiting 0 so it never breaks
# the agent.
POHUNEK_HOOK_ACTION="$action" \
POHUNEK_AGENT_PID="$agent_pid" \
POHUNEK_HOOK_START_MARKER="$start_marker" \
python3 -I - 3<&0 <<'PY' || exit 0
import atexit
import errno
import fcntl
import json
import os
import re
import shlex
import socket
import stat
import sys
import time
import unicodedata
from datetime import datetime, timedelta, timezone

# Every hook event is its own short-lived interpreter, and the worker orders the
# reports of one agent by this value. `time.monotonic_ns()` is only defined
# within one process (Apple's Python 3.9 on macOS 27 counts from its own
# start), so the system-wide monotonic clock is read instead. It is read before
# stdin, so a large provider input never delays the moment it records.
sequence = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
invocation_start = time.monotonic()

agent = "claude"
ACTION_SESSION = "session"
ACTION_RELEASE = "release"
ACTION_SUBAGENT_START = "subagent-start"
ACTION_SUBAGENT_STOP = "subagent-stop"
ACTION_STOP = "stop"
ACTION_PROMPT_SUBMIT = "prompt-submit"
ACTIONS = (
    ACTION_SESSION,
    ACTION_RELEASE,
    ACTION_SUBAGENT_START,
    ACTION_SUBAGENT_STOP,
    ACTION_STOP,
    ACTION_PROMPT_SUBMIT,
)
TIMESTAMP_MS_FACTOR = 1000
SOCKET_TIMEOUT_SECS = 0.5
RESPONSE_BYTES = 4096
MAX_RETRY_MS_DIGITS = 9
MIN_AGENT_PID = 1
IDENTITY_TTL_SECS = 30
# Protocol 3 spells the worker instance key `runtime_id`; protocol 4 renamed it.
# A session keeps the protocol version of its launch, so the spelling follows it.
WORKER_INSTANCE_KEY_RENAMED_AT = 4
# Bounds provider JSON before decoding; oversized payloads are rejected whole.
MAX_HOOK_INPUT_BYTES = 65536
# The main `Stop` input carries the final assistant message and, per background
# task, a description and command of up to 1000 characters each, so 64 KiB is
# too small. 1 MiB holds a long final message plus MAX_SNAPSHOT_TASKS tasks.
MAX_STOP_HOOK_INPUT_BYTES = 1048576
# The prompt-submit input carries the user's prompt. It is read in chunks of
# this size and discarded, so prompt text never reaches a decoder or a log.
PROMPT_DRAIN_CHUNK_BYTES = 65536
# Mirrors the worker's snapshot task bound; a longer list is not sent at all.
MAX_SNAPSHOT_TASKS = 256
# Fields every snapshot task must carry as non-empty strings; one bad task voids the list.
SNAPSHOT_TASK_FIELDS = ("id", "type", "status")
# Optional task fields are sanitized per task and dropped alone when invalid.
# `label` comes from `description` for every type, `program` from `command` for
# shells only, and `agent_type` for subagents only. `command` and its arguments
# are never forwarded.
SHELL_TASK_TYPE = "shell"
SUBAGENT_TASK_TYPE = "subagent"
# Mirrors the worker's subagent type bound; a longer agent_type is dropped from its task.
MAX_SUBAGENT_TYPE_BYTES = 128
# Injected by the worker into the agent environment; a missing or invalid value
# forwards no label or program (fail closed, there is no fallback literal).
ENV_LABEL_MAX_BYTES = "POHUNEK_BACKGROUND_LABEL_MAX_BYTES"
ENV_PROGRAM_MAX_BYTES = "POHUNEK_BACKGROUND_PROGRAM_MAX_BYTES"
# Digits accepted in those limit variables. Nine digits exceed any real bound and
# stay far below the length at which int() raises.
MAX_LIMIT_DIGITS = 9
# Unicode categories removed from a label or program: C0/C1 controls, format
# characters (bidi overrides, zero-width marks), line and paragraph separators,
# and surrogates, which cannot be encoded as UTF-8.
STRIPPED_CATEGORIES = frozenset(("Cc", "Cf", "Zl", "Zp", "Cs"))
# Controls that separate words, replaced by one space before stripping so that
# "a\tb" reads as "a b" rather than "ab".
WORD_SEPARATOR_CHARS = "\t\n\v\f\r"
PROGRAM_NAME_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]*")
ASSIGNMENT_PATTERN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=")
CHAIN_OPERATORS = ("&&", ";")
# `env` options that take a separate value argument; other options stand alone.
ENV_OPTIONS_WITH_VALUE = ("-u", "-C", "--unset", "--chdir")
# `env -S` splits one string into the command line, so no command word is visible.
ENV_SPLIT_OPTIONS = ("-S", "--split-string")
# The only SessionStart sources forwarded to the worker; any other value is
# omitted. `fork` is deliberately absent: a forked session is a new process
# that must not clear the snapshot of the session it was forked from.
SESSION_SOURCES = ("startup", "resume", "clear", "compact")
# The only SessionEnd reasons forwarded to the worker; any other value is omitted.
RELEASE_REASONS = ("clear", "resume", "logout", "prompt_input_exit", "other")
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
# A start marker older than this belongs to a run that is long gone (the hook
# timeout is 10 s) and is removed by the next SessionStart run.
START_MARKER_MAX_AGE_SECONDS = 3600
TRACE_LOCK_ATTEMPTS = 5
TRACE_LOCK_RETRY_SECONDS = 0.005

action = os.environ.get("POHUNEK_HOOK_ACTION")
session_id = os.environ.get("POHUNEK_SESSION_ID")
socket_path = os.environ.get("POHUNEK_SOCKET_PATH")
worker_socket_path = os.environ.get("POHUNEK_WORKER_SOCKET_PATH")
protocol_raw = os.environ.get("POHUNEK_PROTOCOL_VERSION")
worker_instance_id = os.environ.get("POHUNEK_WORKER_INSTANCE_ID")
agent_pid_raw = os.environ.get("POHUNEK_AGENT_PID")
trace_path = os.environ.get("POHUNEK_HOOK_TRACE_PATH")
trace_max_raw = os.environ.get("POHUNEK_HOOK_TRACE_MAX_BYTES")


def strict_positive_int(raw, max_digits):
    # ASCII digits without a leading zero, the form the worker writes; any
    # other value, including an overlong one, is None.
    if (
        raw
        and len(raw) <= max_digits
        and raw.isascii()
        and raw.isdigit()
        and raw[0] != "0"
    ):
        return int(raw)
    return None


start_marker = os.environ.get("POHUNEK_HOOK_START_MARKER")
trace_max_bytes = strict_positive_int(trace_max_raw, MAX_TRACE_BYTES_DIGITS)
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


def clear_start_markers():
    # Removes this run's start marker, now that the interpreter runs, and the
    # markers of runs older than START_MARKER_MAX_AGE_SECONDS. Only regular
    # files of the current user named after the trace are touched, and a
    # failure leaves them for the next run.
    prefix = trace_path + ".start."
    try:
        if start_marker and start_marker.startswith(prefix):
            os.unlink(start_marker)
        cutoff = time.time() - START_MARKER_MAX_AGE_SECONDS
        with os.scandir(os.path.dirname(trace_path)) as entries:
            for entry in entries:
                if not entry.path.startswith(prefix):
                    continue
                found = entry.stat(follow_symlinks=False)
                if (
                    stat.S_ISREG(found.st_mode)
                    and found.st_uid == os.getuid()
                    and found.st_mtime < cutoff
                ):
                    os.unlink(entry.path)
    except OSError:
        pass


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
if action == ACTION_SESSION:
    # Written before any other work, because a hook the agent kills never
    # reaches the exit handler. Handlers run last-in first-out, so the second
    # one queues `hook_finished` after every reason of the run and before the
    # flush; `hook_started` without it names a killed run.
    trace("hook_started")
    flush_trace()
    trace_lines.clear()
    if trace_enabled:
        clear_start_markers()
    atexit.register(lambda: trace("hook_finished"))

if not worker_socket_path and not socket_path:
    trace("worker_socket_missing", "unset")
    raise SystemExit(0)
if action not in ACTIONS:
    trace("action_invalid")
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


def worker_route_problem():
    # The trace reason that keeps a request from the worker, or None.
    if not worker_socket_path:
        return ("worker_socket_missing", "unset")
    if not worker_instance_id:
        return ("worker_instance_missing", None)
    if agent_pid is None:
        return ("agent_pid_invalid", None)
    return None


# A snapshot goes to the worker only; without a worker claim stdin is not read.
if action == ACTION_STOP:
    stop_route_problem = worker_route_problem()
    if stop_route_problem is not None:
        trace(*stop_route_problem)
        raise SystemExit(0)


background_label_max_bytes = strict_positive_int(
    os.environ.get(ENV_LABEL_MAX_BYTES), MAX_LIMIT_DIGITS
)
background_program_max_bytes = strict_positive_int(
    os.environ.get(ENV_PROGRAM_MAX_BYTES), MAX_LIMIT_DIGITS
)


def strip_controls(text):
    # Word separators become spaces, then every stripped category is removed and
    # runs of whitespace collapse to one space.
    spaced = "".join(" " if char in WORD_SEPARATOR_CHARS else char for char in text)
    kept = "".join(
        char for char in spaced if unicodedata.category(char) not in STRIPPED_CATEGORIES
    )
    return " ".join(kept.split())


def truncate_utf8(text, max_bytes):
    # Cuts to at most max_bytes on a character boundary; the trailing space a cut
    # can leave is trimmed.
    encoded = text.encode("utf-8")
    if len(encoded) <= max_bytes:
        return text
    return encoded[:max_bytes].decode("utf-8", errors="ignore").rstrip()


def sanitize_label(description, max_bytes):
    # The task description as a display label, or None when nothing is left.
    if not isinstance(description, str):
        return None
    return truncate_utf8(strip_controls(description), max_bytes) or None


def command_word_index(words):
    # Index of the command word after the launch prefix the shell skips: leading
    # NAME=value assignments, `cd <dir>` followed by `&&` or `;`, and `env` with
    # its options. None when no command word can be identified.
    index = 0
    while index < len(words):
        word = words[index]
        if ASSIGNMENT_PATTERN.match(word):
            index += 1
        elif word == "cd" and index + 2 < len(words) and words[index + 2] in CHAIN_OPERATORS:
            index += 3
        elif word == "env":
            index = env_command_index(words, index + 1)
            if index is None:
                return None
        else:
            return index
    return None


def env_command_index(words, index):
    # Index of the command after the options and assignments of `env`, or None
    # when `env -S` hides it inside one argument.
    while index < len(words):
        option = words[index]
        if option.startswith(ENV_SPLIT_OPTIONS):
            return None
        if option in ENV_OPTIONS_WITH_VALUE:
            index += 2
        elif option.startswith("-") or ASSIGNMENT_PATTERN.match(option):
            index += 1
        else:
            return index
    return None


def sanitize_program(command, max_bytes):
    # The basename of the command word, or None. Arguments are never returned,
    # and an operator or subshell as the command word fails the name pattern.
    # shlex time grows superlinearly with the length of one token (about 32 ms at
    # 64 KiB, about 6 s at 1 MiB), so a command longer than the hook input bound
    # gets no program; a truncated command word could name a different program,
    # so the command is never cut.
    if not isinstance(command, str):
        return None
    if len(command.encode("utf-8", "surrogatepass")) > MAX_HOOK_INPUT_BYTES:
        return None
    lexer = shlex.shlex(command, posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    try:
        words = list(lexer)
    except ValueError:
        return None
    index = command_word_index(words)
    if index is None or index >= len(words):
        return None
    name = words[index].rsplit("/", 1)[-1]
    if PROGRAM_NAME_PATTERN.fullmatch(name) is None or len(name) > max_bytes:
        return None
    return name


def valid_agent_type(value):
    # The worker's subagent text rule (non-empty, within the bound, no controls)
    # plus the hidden characters STRIPPED_CATEGORIES, which a label also removes.
    # An invalid value drops the agent_type field of its task only.
    if not isinstance(value, str) or not value:
        return False
    if any(unicodedata.category(char) in STRIPPED_CATEGORIES for char in value):
        return False
    return len(value.encode("utf-8")) <= MAX_SUBAGENT_TYPE_BYTES


def snapshot_task(entry):
    # One task reduced to SNAPSHOT_TASK_FIELDS plus the optional fields that
    # sanitize. None when a required field is missing; an invalid optional field
    # is left out of this task only.
    task = {field: entry.get(field) for field in SNAPSHOT_TASK_FIELDS}
    if not all(isinstance(value, str) and value for value in task.values()):
        return None
    if background_label_max_bytes is not None:
        label = sanitize_label(entry.get("description"), background_label_max_bytes)
        if label is not None:
            task["label"] = label
    if task["type"] == SHELL_TASK_TYPE and background_program_max_bytes is not None:
        program = sanitize_program(entry.get("command"), background_program_max_bytes)
        if program is not None:
            task["program"] = program
    if task["type"] == SUBAGENT_TASK_TYPE and valid_agent_type(entry.get("agent_type")):
        task["agent_type"] = entry["agent_type"]
    return task


def snapshot_tasks(parsed):
    # The main agent's background tasks reduced to the snapshot fields, or None
    # and the reason no snapshot is sent. A missing or malformed list means
    # "no snapshot", never "nothing running", so one bad required field voids the
    # whole list instead of being skipped.
    agent_id = parsed.get("agent_id")
    if isinstance(agent_id, str) and agent_id:
        return None, "subagent_stop"
    entries = parsed.get("background_tasks")
    if entries is None:
        return None, "tasks_missing"
    if not isinstance(entries, list):
        return None, "tasks_invalid"
    if len(entries) > MAX_SNAPSHOT_TASKS:
        return None, "tasks_over_bound"
    tasks = []
    for entry in entries:
        if not isinstance(entry, dict):
            return None, "tasks_invalid"
        task = snapshot_task(entry)
        if task is None:
            return None, "tasks_invalid"
        tasks.append(task)
    return tasks, None


def drain_prompt_input():
    # Reads the prompt to its end without keeping or decoding any of it, so the
    # agent never blocks on a full pipe and an oversized prompt still reports.
    try:
        with os.fdopen(3, "rb") as handle:
            while handle.read(PROMPT_DRAIN_CHUNK_BYTES):
                pass
    except OSError:
        pass


def read_hook_input():
    # The fields this action needs and None, or {} and the trace reason and
    # detail that voided the input.
    if action == ACTION_PROMPT_SUBMIT:
        drain_prompt_input()
        return {}, None
    limit = MAX_STOP_HOOK_INPUT_BYTES if action == ACTION_STOP else MAX_HOOK_INPUT_BYTES
    try:
        with os.fdopen(3, "rb") as handle:
            content = handle.read(limit + 1)
    except OSError as error:
        return {}, ("input_invalid", errno_name(error))
    if len(content) > limit:
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
    if action == ACTION_STOP:
        # The identity fields are returned even when the task list is void, so
        # an unusable snapshot never costs the turn's identity report.
        identity = {field: parsed.get(field) for field in ("session_id", "transcript_path")}
        tasks, no_snapshot = snapshot_tasks(parsed)
        if tasks is None:
            return identity, ("no_snapshot", no_snapshot)
        return {**identity, "tasks": tasks}, None
    fields = (
        ("agent_id", "agent_type", "parent_agent_id")
        if action in (ACTION_SUBAGENT_START, ACTION_SUBAGENT_STOP)
        else ("session_id", "transcript_path", "source", "reason")
    )
    return {field: parsed.get(field) for field in fields}, None


hook_input, input_problem = read_hook_input()


def missing_input(detail):
    # The input's own problem when it was voided, else the missing field.
    return input_problem if input_problem is not None else ("input_invalid", detail)


native = hook_input.get("session_id")
native_session_id = native if isinstance(native, str) and native else None
transcript = hook_input.get("transcript_path")
transcript_path = transcript if isinstance(transcript, str) and transcript else None

timestamp_ms = int(time.time() * TIMESTAMP_MS_FACTOR)


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
        return False
    if answer.get("ok") is True:
        return True
    trace("worker_refused")
    return False


def send_worker_hook(request_type):
    problem = worker_route_problem()
    if problem is not None:
        trace(*problem)
        return False
    start_identity = agent_start_identity()
    if start_identity is None:
        return False
    request = {
        "type": request_type,
        "runtime_id": worker_instance_id,
        "provider": agent,
        "pid": agent_pid,
        "start_identity": start_identity,
        "sequence": sequence,
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
        # The worker reads the source only to clear a background snapshot on
        # startup or resume; the key is absent for any other value.
        source = hook_input.get("source")
        if isinstance(source, str) and source in SESSION_SOURCES:
            request["session_source"] = source
    if request_type == "identity_release":
        # The worker keeps the background snapshot when the session ends for a
        # `/clear`, which Claude follows with a SessionStart in the same process.
        reason = hook_input.get("reason")
        if isinstance(reason, str) and reason in RELEASE_REASONS:
            request["release_reason"] = reason
    # Pending launch verification is durably owned and retried by the worker.
    return send_worker(request)


def send_worker_subagent(request_type):
    problem = worker_route_problem()
    if problem is not None:
        trace(*problem)
        return
    subagent_id = hook_input.get("agent_id")
    if not isinstance(subagent_id, str) or not subagent_id:
        trace(*missing_input("agent_id_missing"))
        return
    start_identity = agent_start_identity()
    if start_identity is None:
        return
    agent_type = hook_input.get("agent_type")
    parent_id = hook_input.get("parent_agent_id")
    request = {
        "type": request_type,
        "runtime_id": worker_instance_id,
        "provider": agent,
        "pid": agent_pid,
        "start_identity": start_identity,
        "sequence": sequence,
        "subagent_id": subagent_id,
    }
    if request_type == "subagent_start":
        request["agent_type"] = agent_type if isinstance(agent_type, str) else None
        request["parent_id"] = parent_id if isinstance(parent_id, str) else None
    else:
        request["outcome"] = "completed"
    send_worker(request)


def send_worker_snapshot(tasks):
    # No daemon fallback exists for a snapshot.
    start_identity = agent_start_identity()
    if start_identity is None:
        return
    send_worker({
        "type": "subagent_snapshot",
        "runtime_id": worker_instance_id,
        "provider": agent,
        "pid": agent_pid,
        "start_identity": start_identity,
        "sequence": sequence,
        "tasks": tasks,
    })


def send_worker_prompt():
    # Reports prompt acceptance to the worker once. The prompt waits on this
    # hook, so there is no retry and no daemon fallback.
    problem = worker_route_problem()
    if problem is not None:
        trace(*problem)
        return
    start_identity = agent_start_identity()
    if start_identity is None:
        return
    send_worker({
        "type": "prompt_accepted",
        "runtime_id": worker_instance_id,
        "provider": agent,
        "pid": agent_pid,
        "start_identity": start_identity,
        "sequence": sequence,
        "expires_at": (
            datetime.now(timezone.utc) + timedelta(seconds=IDENTITY_TTL_SECS)
        ).isoformat().replace("+00:00", "Z"),
    })


def send_request(method, params, suffix):
    # Sends one daemon request; True when the daemon asked for a retry.
    if retry is not None:
        params = dict(params, retryable_admission=True)
    request = {
        "v": {"minimum": protocol_version, "maximum": protocol_version},
        "id": f"hook:{agent}:{timestamp_ms}:{suffix}",
        "method": method,
        "params": params,
    }
    answer = exchange(socket_path, request, "daemon")
    error = answer.get("err") if answer is not None else None
    if error is None:
        return False
    if retry is not None and isinstance(error, dict) and error.get("code") == RETRYABLE_ADMISSION_CODE:
        return True
    trace("daemon_refused")
    return False


RETRYABLE_ADMISSION_CODE = "hook_schema_unavailable"


def retry_policy():
    # (deadline, first pause, longest pause) in seconds. The daemon announces
    # support and the values through the launch environment; without all of
    # them the report is sent once, as before.
    if os.environ.get("POHUNEK_HOOK_ADMISSION_RETRY") != "1":
        return None
    values = [
        strict_positive_int(os.environ.get(name), MAX_RETRY_MS_DIGITS)
        for name in (
            "POHUNEK_HOOK_RETRY_DEADLINE_MS",
            "POHUNEK_HOOK_RETRY_BACKOFF_MS",
            "POHUNEK_HOOK_RETRY_BACKOFF_MAX_MS",
        )
    ]
    if any(value is None for value in values):
        return None
    return tuple(value / 1000 for value in values)


retry = retry_policy()


def deliver_with_retry(attempt):
    # `attempt` runs one whole delivery, the worker route first and the daemon
    # fallback after it, and returns True when the daemon asked for a retry
    # because the session's worker was unreachable. A retry waits a doubling
    # pause until the deadline counted from the start of the invocation. Every
    # attempt keeps the invocation's sequence so a late retry cannot outrank a
    # newer report.
    pause = retry[1] if retry else 0
    while attempt():
        if retry is None:
            return
        if time.monotonic() - invocation_start + pause >= retry[0]:
            trace("daemon_retry_exhausted")
            return
        trace("daemon_retry")
        time.sleep(pause)
        pause = min(pause * 2, retry[2])


def daemon_unusable():
    # True when the daemon fallback cannot run. With no worker configured the
    # daemon route is the only one, so its invalid protocol version is the
    # reason; a configured worker has already traced its own failure.
    if socket_path and protocol_version is not None:
        return False
    if not worker_socket_path:
        trace("daemon_protocol_invalid")
    return True


if action == ACTION_SUBAGENT_START:
    send_worker_subagent("subagent_start")
    raise SystemExit(0)

if action == ACTION_SUBAGENT_STOP:
    send_worker_subagent("subagent_stop")
    raise SystemExit(0)

if action == ACTION_PROMPT_SUBMIT:
    # The agent injects this hook's output into the prompt context, so nothing
    # reaches stdout or stderr whatever fails.
    try:
        send_worker_prompt()
    except Exception:
        trace("prompt_report_failed")
    raise SystemExit(0)

if action == ACTION_STOP:
    # The end of a turn repeats the identity report: it is safe to repeat,
    # ordered by `sequence`, and it recovers a SessionStart report that was lost
    # because the agent killed that hook. The worker is the only route.
    if native_session_id:
        send_worker_hook("identity_report")
    if input_problem is not None:
        trace(*input_problem)
        raise SystemExit(0)
    # A report of another process clears the snapshot of the previous owner at
    # the worker's clock, so the snapshot is ordered after that report.
    sequence = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
    send_worker_snapshot(hook_input["tasks"])
    raise SystemExit(0)

def once(build):
    # Builds a value on first use and returns the same one on every retry, so a
    # retried report keeps the identifiers of its first attempt.
    cache = []

    def get():
        if not cache:
            cache.append(build())
        return cache[0]

    return get


def release_params():
    params = {
        "session_id": session_id,
        "source": f"pohunek:{agent}",
        "agent": agent,
        "seq": str(timestamp_ms),
        "monotonic_sequence": str(sequence),
    }
    if agent_pid is not None:
        params["pid"] = agent_pid
        start_identity = agent_start_identity()
        if start_identity is not None:
            params["pid_start_identity"] = str(start_identity)
    return params


if action == ACTION_RELEASE:
    release_agent_params = once(release_params)

    def release_attempt():
        if worker_socket_path and send_worker_hook("identity_release"):
            return False
        if daemon_unusable():
            return False
        return send_request("session.release_agent", release_agent_params(), "release")

    deliver_with_retry(release_attempt)
    raise SystemExit(0)

if not native_session_id:
    trace(*missing_input("session_id_missing"))
    raise SystemExit(0)


def agent_params():
    params = {
        "session_id": session_id,
        "source": f"pohunek:{agent}",
        "agent": agent,
        "seq": str(timestamp_ms),
        "agent_session_id": native_session_id,
    }
    params["monotonic_sequence"] = str(sequence)
    if agent_pid is not None:
        params["pid"] = agent_pid
        start_identity = agent_start_identity()
        if start_identity is not None:
            params["pid_start_identity"] = str(start_identity)
    if transcript_path:
        params["agent_session_path"] = transcript_path
    return params


def native_params():
    # None when the worker instance or the agent identity is unknown.
    if not (worker_instance_id and agent_pid is not None):
        return None
    start_identity = agent_start_identity()
    if start_identity is None:
        return None
    worker_instance_key = (
        "worker_instance_id"
        if protocol_version >= WORKER_INSTANCE_KEY_RENAMED_AT
        else "runtime_id"
    )
    params = {
        "session_id": session_id,
        worker_instance_key: worker_instance_id,
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
        params["transcript_path"] = transcript_path
    return params


report_agent_params = once(agent_params)
report_native_params = once(native_params)


def report_attempt():
    if worker_socket_path and send_worker_hook("identity_report"):
        return False
    if daemon_unusable():
        return False
    if send_request("session.report_agent", report_agent_params(), "agent"):
        return True
    native_id_params = report_native_params()
    if native_id_params is None:
        return False
    return send_request("session.report_native_id", native_id_params, "native")


deliver_with_retry(report_attempt)
PY
