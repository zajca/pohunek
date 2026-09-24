#!/bin/sh
# Install or upgrade the pohunek daemon service from this release archive.
#
# `pohunek service install|upgrade` does the work: versioned binaries under
# <prefix>/libexec/pohunek/<version>/, service.toml, the daemon job, and the
# readiness check. This wrapper only locates the staged binaries next to it,
# retires a pre-service install (a `pohunekd.service` unit with template
# workers) after the one-time migration preflight, and picks the subcommand:
#
# - `service install` while `pohunek service status --json` reports a pending
#   install transaction: install resumes it (same version and prefix) or rolls
#   it back and installs afresh. That install already wrote service.toml once
#   it passed its `config` step, and `service upgrade` refuses such a record;
# - otherwise `service upgrade` when service.toml exists;
# - otherwise `service install`.

set -eu

usage() {
    echo "usage: $0 [--accept-runtime-loss]" >&2
}

accept_runtime_loss=0
if [ "${1:-}" = "--accept-runtime-loss" ]; then
    accept_runtime_loss=1
    shift
fi
if [ "$#" -ne 0 ]; then
    usage
    exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
archive_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
prefix=${POHUNEK_INSTALL_PREFIX:-"$HOME/.local"}
config_home=${XDG_CONFIG_HOME:-"$HOME/.config"}
service_config="$config_home/pohunek/service.toml"
legacy_unit_dir="$config_home/systemd/user"

for required in pohunek pohunekd pohunek-sessiond; do
    if [ ! -f "$archive_dir/$required" ] || [ ! -x "$archive_dir/$required" ]; then
        echo "required staged binary is missing or not executable: $archive_dir/$required" >&2
        exit 1
    fi
done

# Asked before anything changes, so a failing query leaves the host untouched.
# The status report is pretty-printed JSON in which only `pending_transaction`
# carries an "operation" key. A `--json` failure prints its error document on
# stdout, so the captured output is forwarded to stderr.
if ! status_json=$("$archive_dir/pohunek" service status --json); then
    printf '%s\n' "$status_json" >&2
    echo "\`pohunek service status --json\` failed; nothing was changed" >&2
    echo "fix the reported problem and re-run $0" >&2
    exit 1
fi
if ! printf '%s\n' "$status_json" | grep -q '"pending_transaction"'; then
    echo "\`pohunek service status --json\` did not report pending_transaction; nothing was changed" >&2
    exit 1
fi
pending_install=0
if printf '%s\n' "$status_json" \
    | grep -Eq '"operation"[[:space:]]*:[[:space:]]*"install"'; then
    pending_install=1
fi

# A pre-service install runs `pohunekd.service` from <prefix>/bin with
# `pohunek-session@.service` template workers. Its daemon holds the instance
# locks the service daemon needs, so it is retired first, in this order:
# preflight, active-worker check, disable, removal of the exact legacy paths,
# daemon-reload. Every step tolerates a previous partial run: the preflight
# only runs while the legacy daemon is still active, and removal and disable
# are no-ops for what is already gone.
legacy_retired=0
if [ -e "$legacy_unit_dir/pohunekd.service" ]; then
    # The archive's own CLI runs the preflight, which refuses while the legacy
    # daemon owns live PTYs.
    if systemctl --user is-active --quiet pohunekd.service; then
        if [ "$accept_runtime_loss" -eq 1 ]; then
            "$archive_dir/pohunek" migration preflight --accept-runtime-loss
        else
            "$archive_dir/pohunek" migration preflight
        fi
    fi
    live_workers=$(systemctl --user list-units 'pohunek-session@*' \
        --state=active --plain --no-legend)
    if [ -n "$live_workers" ]; then
        echo "legacy template workers are still running; nothing was changed:" >&2
        echo "$live_workers" >&2
        echo "stop each of these sessions with \`pohunek session stop <id>\` and re-run $0" >&2
        exit 1
    fi
    systemctl --user disable --now pohunekd.service
    rm -f \
        "$legacy_unit_dir/pohunekd.service" \
        "$legacy_unit_dir/pohunek-session@.service" \
        "$legacy_unit_dir/pohunek-sessions.slice"
    systemctl --user daemon-reload
    legacy_retired=1
fi
if [ -e "$prefix/bin/pohunekd" ] || [ -e "$prefix/libexec/pohunek-sessiond" ]; then
    rm -f "$prefix/bin/pohunekd" "$prefix/libexec/pohunek-sessiond"
    legacy_retired=1
fi

if [ "$pending_install" -eq 1 ]; then
    set -- service install --from "$archive_dir" --prefix "$prefix"
elif [ -f "$service_config" ]; then
    set -- service upgrade --from "$archive_dir"
else
    set -- service install --from "$archive_dir" --prefix "$prefix"
fi
status=0
"$archive_dir/pohunek" "$@" || status=$?
if [ "$status" -ne 0 ] && [ "$legacy_retired" -eq 1 ]; then
    echo "the legacy pohunekd.service install was already retired before \`pohunek $1 $2\` failed;" >&2
    echo "fix the reported problem and re-run $0 to finish the installation" >&2
fi
exit "$status"
