#!/bin/sh
# Records the real Claude Code TUI against a loopback Messages stub into
# fixtures/claude.trace.
#
# usage: record-claude.sh <record-trace binary> <scratch dir> <claude binary>
#
# <claude binary> is the release pinned in compat/claude/npm (compat/claude/
# compatibility-lock.json), installed outside the repository. The recording
# runs in an isolated HOME and CLAUDE_CONFIG_DIR with a dummy API key, the
# updater, telemetry and every non-essential request switched off, and the stub
# as the only endpoint. Steps: wait for the prompt, type a message, wait for
# the stub's reply, leave with /exit. Review the trace for paths, hostnames and
# secrets before committing it.
set -eu
recorder=$1
scratch=$2
claude=$3
here=$(cd "$(dirname "$0")" && pwd)
home="$scratch/claude-home"
project="$home/project"
rm -rf -- "$home"
mkdir -p "$project" "$home/config"

port_file="$scratch/stub.port"
rm -f -- "$port_file"
python3 -I "$here/claude-messages-stub.py" "$port_file" &
stub=$!
trap 'kill "$stub" 2>/dev/null || true' EXIT
while [ ! -s "$port_file" ]; do sleep 0.1; done
port=$(cat "$port_file")

# Dummy key: only its last 20 characters are recorded as approved.
key="spike-stub-key-0000000000000000000"
suffix=$(printf '%s' "$key" | tail -c 20)
cat >"$home/config/.claude.json" <<EOF
{"hasCompletedOnboarding": true,
 "customApiKeyResponses": {"approved": ["$suffix"], "rejected": []},
 "projects": {"$project": {"hasTrustDialogAccepted": true}}}
EOF
printf '{"theme": "dark"}\n' >"$home/config/settings.json"

"$recorder" --out "$here/../claude.trace" --home "$home" --cwd "$project" \
    --rows 30 --cols 100 \
    --env "PATH=$(dirname "$claude"):/usr/bin:/bin" \
    --env "CLAUDE_CONFIG_DIR=$home/config" \
    --env "ANTHROPIC_BASE_URL=http://127.0.0.1:$port" \
    --env "ANTHROPIC_API_KEY=$key" \
    --env "ANTHROPIC_MODEL=claude-sonnet-4-5" \
    --env DISABLE_AUTOUPDATER=1 --env DISABLE_UPDATES=1 --env DISABLE_TELEMETRY=1 \
    --env DISABLE_ERROR_REPORTING=1 --env CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
    --env CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL=1 \
    --on 'for agents=hello there\r' \
    --on 'paragraphs=/exit\r' \
    -- "$claude"
