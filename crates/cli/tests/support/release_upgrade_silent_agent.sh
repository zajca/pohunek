#!/bin/sh
# Silent `claude` reader for the release upgrade test. The test writes it with
# the same interpreter line and `FAKE_AGENT_VERSION_LINE` assignment as the
# fake agent, and `--version` answers the package's version probe.
if [ "${1:-}" = --version ]; then
    printf '%s\n' "$FAKE_AGENT_VERSION_LINE"
    exit 0
fi
# The only output before the first prompt is Claude's idle prompt box: a rule,
# the `❯` line and a rule, which the package's `live_prompt_box` rule reads as
# ready for the initial input. The previous release's client has a five-second
# request deadline for session creation, so nothing else delays the prompt.
rule=''
i=0
while [ "$i" -lt 24 ]; do
    rule="$rule$(printf '\342\224\200')"
    i=$((i + 1))
done
printf '\033[2J\033[H%s\r\n\342\235\257 \r\n%s\r\n? for shortcuts\r\n' "$rule" "$rule"
IFS= read -r line || exit 1
printf 'silent-agent ack %s\n' "$line"
while IFS= read -r line; do :; done
