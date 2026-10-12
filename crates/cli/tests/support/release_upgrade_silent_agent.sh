#!/bin/sh
# Silent `claude` reader for the release upgrade test. The test writes it with
# the same interpreter line and `FAKE_CLAUDE_VERSION_LINE` assignment as the
# fake agent, and `--version` answers the package's version probe.
if [ "${1:-}" = --version ]; then
    printf '%s\n' "$FAKE_CLAUDE_VERSION_LINE"
    exit 0
fi
# Do not emit readiness output before accepting the first prompt. The previous
# release's client has a five-second request deadline for session creation.
IFS= read -r line || exit 1
printf 'silent-agent ack %s\n' "$line"
while IFS= read -r line; do :; done
