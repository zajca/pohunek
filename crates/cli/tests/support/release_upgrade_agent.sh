#!/bin/sh
# Fake `claude` agent for the release upgrade test.
#
# The test writes this file with its first line replaced by an interpreter
# line naming a copy of the shell called `fake-claude-sh`, and with a
# `FAKE_AGENT_VERSION_LINE` assignment after it. The process image is then
# that copy, whose name the worker accepts as a claude launch, and the profile
# launches this script directly, so the package's version probe runs it too.
#
# `--version` prints the version line the installed package's probe accepts.
# It is answered before anything else: the probe runs with an empty
# environment, a throwaway HOME and no profile variables.
#
# Otherwise it is the PTY child of the session and runs the managed hook
# scripts the installed release wrote, as its direct children, so each hook
# reports the PID of this process as the agent. Configuration comes from the
# host profile environment:
#   FAKE_AGENT_LOG        file receiving one line per launch
#   FAKE_AGENT_HOOKS      directory holding the installed hook scripts
#   FAKE_AGENT_NATIVE_ID  native id reported by a fresh launch
# A launch with `--resume <id>` reports that id again, as a resumed claude does.
#
# Commands read from the terminal, one per line:
#   echo:<text>      prints `fake-agent ack <text>`
#   report:<id>      reports <id> as the native session id through the state hook
#   stopfail:<event> sends an error notification with hook event id <event>
#   quit             exits

if [ "${1:-}" = --version ]; then
    printf '%s\n' "$FAKE_AGENT_VERSION_LINE"
    exit 0
fi

set -u

: "${FAKE_AGENT_LOG:?}" "${FAKE_AGENT_HOOKS:?}" "${FAKE_AGENT_NATIVE_ID:?}" "${HOME:?}"

umask 077

hook_input=$(mktemp) || exit 1
trap 'rm -f "$hook_input"' EXIT HUP INT TERM

# Hooks read their JSON payload from a file, not a pipe, so that no extra
# process sits between this shell and the hook script.
# Each reported id names a regular transcript in the fixture's Claude store.
report_native() {
    case "$1" in
        ''|*[!A-Za-z0-9_-]*) exit 1 ;;
    esac
    transcript_dir="$HOME/.claude/projects/upgrade-fixture"
    mkdir -p "$transcript_dir" || exit 1
    printf '{}\n' >"$transcript_dir/$1.jsonl" || exit 1
    printf '{"session_id":"%s"}\n' "$1" >"$hook_input"
    "$FAKE_AGENT_HOOKS/pohunek-agent-state.sh" session <"$hook_input"
}

# Usage: notify <action> <matcher-or-empty> <hook event id>
notify() {
    printf '{"hook_event_id":"%s"}\n' "$3" >"$hook_input"
    if [ -n "$2" ]; then
        "$FAKE_AGENT_HOOKS/pohunek-agent-notify.sh" "$1" "$2" <"$hook_input"
    else
        "$FAKE_AGENT_HOOKS/pohunek-agent-notify.sh" "$1" <"$hook_input"
    fi
}

launch_args="$*"
resume_ref=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --resume)
            resume_ref="${2:-}"
            shift
            ;;
    esac
    shift
done

if [ -n "$resume_ref" ]; then
    native_id="$resume_ref"
    launch_event="evt-after-resume"
else
    native_id="$FAKE_AGENT_NATIVE_ID"
    launch_event="evt-before-upgrade"
fi

printf 'launch args=[%s] native=%s\n' "$launch_args" "$native_id" >>"$FAKE_AGENT_LOG"
printf 'fake-agent started args=[%s]\n' "$launch_args"
report_native "$native_id"
# An error notification, not a permission prompt: the daemon debounces an
# attention notification and drops it once the session starts working, which
# this agent's own output can trigger at launch.
notify stop_failure "" "$launch_event"
printf 'fake-agent ready native=%s\n' "$native_id"

while IFS= read -r line; do
    case "$line" in
        echo:*)
            printf 'fake-agent ack %s\n' "${line#echo:}"
            ;;
        report:*)
            native_id="${line#report:}"
            report_native "$native_id"
            printf 'fake-agent reported %s\n' "$native_id"
            ;;
        stopfail:*)
            notify stop_failure "" "${line#stopfail:}"
            printf 'fake-agent notified %s\n' "${line#stopfail:}"
            ;;
        quit)
            exit 0
            ;;
    esac
done
