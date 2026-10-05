#!/bin/sh
# Fake `claude` agent for the release upgrade test.
#
# It is the PTY child of the session and runs the managed hook scripts the
# installed release wrote, as its direct children, so each hook reports the
# PID of this process as the agent. Configuration comes from the host profile
# environment:
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

set -u

: "${FAKE_AGENT_LOG:?}" "${FAKE_AGENT_HOOKS:?}" "${FAKE_AGENT_NATIVE_ID:?}"

hook_input=$(mktemp) || exit 1
trap 'rm -f "$hook_input"' EXIT HUP INT TERM

# Hooks read their JSON payload from a file, not a pipe, so that no extra
# process sits between this shell and the hook script.
report_native() {
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
notify notification permission_prompt "$launch_event"
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
