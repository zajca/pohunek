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

# Subdirectory below the runtime directory and control-socket file name the
# daemon binds, per the shared path contract in crates/paths (`APP_DIR` and
# `SOCKET_NAME`); changing either must stay in step with that crate.
POHUNEK_RUNTIME_SUBDIR=pohunek
POHUNEK_SOCKET_NAME=daemon.sock

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
# socket rename as the connect barrier, preflight over the moved socket,
# worker inventory, disable, removal of the exact legacy paths, daemon-reload.
# The legacy binary is already deployed, so the wrapper cannot add a
# daemon-side barrier to it; the connect barrier closes the only open door and
# every later step fails closed without removing legacy files. Every step
# tolerates a previous partial run: the preflight only runs while the legacy
# daemon is still active, and removal and disable are no-ops for what is
# already gone.
#
# Residual windows the wrapper cannot close without a legacy-side change:
# a client that already holds a control connection (a long-lived GUI) may
# still issue session requests between the barrier and the daemon stop, and
# no offline scan of the legacy persisted store covers such a stop-only
# session; its template worker is caught by the post-stop inventory instead.
restore_barrier() {
    # The operator can restart the left-in-place legacy install, so put the
    # moved socket node back where the daemon expects it whenever that name
    # is vacant.
    if [ -n "$barrier_socket" ] && [ ! -e "$legacy_socket" ]; then
        mv "$barrier_socket" "$legacy_socket" || true
    fi
}
# After the legacy daemon stopped, a moved node is stale: no daemon listens on
# it, and the next install binds a fresh socket at the original path, so the
# node is removed instead of restored.
remove_barrier() {
    if [ -n "$barrier_socket" ]; then
        rm -f "$barrier_socket"
    fi
}
legacy_retired=0
if [ -e "$legacy_unit_dir/pohunekd.service" ]; then
    legacy_socket="${XDG_RUNTIME_DIR:-}/$POHUNEK_RUNTIME_SUBDIR/$POHUNEK_SOCKET_NAME"
    barrier_socket=
    if systemctl --user is-active --quiet pohunekd.service; then
        # Move the daemon's listening socket node away with rename(2): its
        # bound socket keeps serving established connections, while any new
        # client cannot connect, so no new session can start after the
        # preflight that follows.
        if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ ! -S "$legacy_socket" ]; then
            echo "the active legacy daemon control socket is missing: $legacy_socket" >&2
            echo "nothing was changed; stop the legacy daemon and re-run $0" >&2
            exit 1
        fi
        barrier_socket="$legacy_socket.retiring.$$"
        if ! mv "$legacy_socket" "$barrier_socket"; then
            echo "could not move the legacy daemon socket aside; nothing was changed" >&2
            exit 1
        fi
        # The archive's own CLI runs the preflight over the moved socket. It
        # refuses while the legacy daemon owns live PTYs.
        preflight_status=0
        if [ "$accept_runtime_loss" -eq 1 ]; then
            "$archive_dir/pohunek" migration preflight \
                --socket "$barrier_socket" --accept-runtime-loss || preflight_status=$?
        else
            "$archive_dir/pohunek" migration preflight \
                --socket "$barrier_socket" || preflight_status=$?
        fi
        if [ "$preflight_status" -ne 0 ]; then
            restore_barrier
            exit "$preflight_status"
        fi
    fi
    # Fail-closed inventory over every template-worker state the stop could
    # still destroy: anything not `inactive` keeps or is acquiring a PTY,
    # including workers sitting in `activating`.
    legacy_list_workers() {
        systemctl --user list-units 'pohunek-session@*' \
            --all --plain --no-legend \
            | awk '$3 != "inactive" && NF >= 3'
    }
    before_stop_workers=$(legacy_list_workers)
    if [ -n "$before_stop_workers" ]; then
        echo "legacy template workers are still running; nothing was changed:" >&2
        echo "$before_stop_workers" >&2
        echo "stop each of these sessions with \`pohunek session stop <id>\` and re-run $0" >&2
        echo "(a stopped legacy daemon must be started first: \`systemctl --user start pohunekd.service\`)" >&2
        if [ -n "$(printf '%s\n' "$before_stop_workers" \
            | awk '$3 == "failed" {print $1}')" ]; then
            echo "for a unit in the \`failed\` state, confirm that no process remains, then" >&2
            echo "\`systemctl --user reset-failed <unit>\`" >&2
        fi
        restore_barrier
        exit 1
    fi
    systemctl --user disable --now pohunekd.service
    remove_barrier
    # The daemon is stopped and disabled, so its socket is closed and no
    # client can start a session anymore. Re-check the workers it could have
    # spawned since the preflight; anything found here was created inside
    # that window. `--accept-runtime-loss` does not cover it: like the
    # inventory before the stop, a live template worker always blocks the
    # removal of the unit files it runs from.
    after_stop_workers=$(legacy_list_workers)
    if [ -n "$after_stop_workers" ]; then
        echo "the legacy daemon is stopped and disabled, and these template workers" >&2
        echo "appeared after the preflight, so their runtime may be lost:" >&2
        echo "$after_stop_workers" >&2
        echo "the legacy unit files were kept:" >&2
        echo "  $legacy_unit_dir/pohunekd.service" >&2
        echo "  $legacy_unit_dir/pohunek-session@.service" >&2
        echo "  $legacy_unit_dir/pohunek-sessions.slice" >&2
        echo "start the legacy daemon with \`systemctl --user enable --now pohunekd.service\`," >&2
        echo "stop these sessions with \`pohunek session stop <id>\`, and re-run $0;" >&2
        echo "for a unit in the \`failed\` state, confirm that no process remains, then" >&2
        echo "\`systemctl --user reset-failed <unit>\`" >&2
        exit 1
    fi
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
