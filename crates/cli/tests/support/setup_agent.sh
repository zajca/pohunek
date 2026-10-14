# Body of a fake agent for the host setup scenario.
#
# The test writes this file after a shebang line that names a copy of the
# shell called `fake-<agent>-sh`, and after a `--version` answer that the
# installed package's probe accepts. The copy's name is what qualifies the
# process for the worker, which designates an agent by its executable name.
#
# On launch the agent fires the SessionStart hook the installed integration
# wrote (`pohunek-agent-state.sh` in the agent's config home, named by
# FAKE_AGENT_HOOK_DIR), reporting FAKE_AGENT_NATIVE_ID as the native id, and
# appends one line to FAKE_AGENT_LOG. It then waits for `quit` on the terminal.

set -u

: "${FAKE_AGENT_LOG:?}" "${FAKE_AGENT_HOOK_DIR:?}" "${FAKE_AGENT_NATIVE_ID:?}"

umask 077

hook_input=$(mktemp) || exit 1
trap 'rm -f "$hook_input"' EXIT HUP INT TERM

# Hooks read their JSON payload from a file, not a pipe.
printf '{"session_id":"%s"}\n' "$FAKE_AGENT_NATIVE_ID" >"$hook_input"
printf 'launch native=%s\n' "$FAKE_AGENT_NATIVE_ID" >>"$FAKE_AGENT_LOG"
"$FAKE_AGENT_HOOK_DIR/pohunek-agent-state.sh" session <"$hook_input" || exit 1
printf 'fake-agent ready native=%s\n' "$FAKE_AGENT_NATIVE_ID"

while IFS= read -r line; do
    case "$line" in
        quit)
            exit 0
            ;;
    esac
done
