#!/bin/sh
# Records every fixtures/sources/*.sh into fixtures/NAME.trace.
#
# usage: record-all.sh <record-trace binary> <scratch dir>
#
# Each source states its recorder arguments in a `# record:` line. The sources
# run with an isolated HOME under the scratch dir; the real agent trace is
# recorded by record-claude.sh.
set -eu
recorder=$1
scratch=$2
here=$(cd "$(dirname "$0")" && pwd)
for source in "$here"/*.sh; do
    name=$(basename "$source" .sh)
    case "$name" in record-all | record-claude) continue ;; esac
    arguments=$(sed -n 's/^# record: //p' "$source")
    home="$scratch/home-$name"
    rm -rf -- "$home"
    mkdir -p "$home"
    # shellcheck disable=SC2086
    "$recorder" --out "$here/../$name.trace" --home "$home" $arguments -- /bin/sh "$source"
done
