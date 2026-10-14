#!/bin/sh
# Fake `codex` agent for the release upgrade test.
#
# The test writes this file with its first line replaced by an interpreter
# line naming a copy of the shell called `fake-codex-sh`, and with a
# `FAKE_AGENT_VERSION_LINE` assignment after it. The process image is then
# that copy, whose name the worker accepts as a codex launch, and the profile
# launches this script directly, so the package's version probe runs it too.
#
# `--version` prints the version line the installed package's probe accepts.
# It is answered before anything else, as the fake Claude agent answers it.
#
# Otherwise it is the PTY child of the session. It prints a ready line and
# acknowledges input, one command per line:
#   echo:<text>      prints `fake-codex ack <text>`
#   quit             exits

if [ "${1:-}" = --version ]; then
    printf '%s\n' "$FAKE_AGENT_VERSION_LINE"
    exit 0
fi

printf 'fake-codex ready\n'

while IFS= read -r line; do
    case "$line" in
        echo:*)
            printf 'fake-codex ack %s\n' "${line#echo:}"
            ;;
        quit)
            exit 0
            ;;
    esac
done
