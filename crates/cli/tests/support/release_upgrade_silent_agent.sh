#!/bin/sh
# Do not emit readiness output before accepting the first prompt. The previous
# release's client has a five-second request deadline for session creation.
IFS= read -r line || exit 1
printf 'silent-agent ack %s\n' "$line"
while IFS= read -r line; do :; done
