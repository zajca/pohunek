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
# tolerates a previous partial run: the barrier and preflight are skipped only
# for a daemon `systemctl` reports stopped, and removal and disable are no-ops
# for what is already gone.
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
    if [ -n "$barrier_socket" ] && [ -e "$barrier_socket" ] \
        && [ ! -e "$legacy_socket" ]; then
        mv "$barrier_socket" "$legacy_socket" || true
    fi
    disarm_barrier
}
# After the legacy daemon stopped, a moved node is stale: no daemon listens on
# it, and the next install binds a fresh socket at the original path, so the
# node is removed instead of restored.
remove_barrier() {
    if [ -n "$barrier_socket" ]; then
        rm -f "$barrier_socket"
    fi
    disarm_barrier
}
# A settled barrier needs no cleanup on exit or interruption.
disarm_barrier() {
    barrier_socket=
    trap - EXIT HUP INT TERM
}
# Sets `legacy_state` to the state `systemctl --user is-active` prints for the
# legacy daemon. Its exit status is non-zero both for every state but `active`
# and for a failed query, so only the printed state is trusted: an empty
# answer is a failed query (status 1). Only `inactive` or `failed` proves the
# daemon stopped; every other state (activating, deactivating, reloading, an
# unknown word) may have a running daemon with clients.
query_legacy_state() {
    legacy_state=$(systemctl --user is-active pohunekd.service) || :
    [ -n "$legacy_state" ]
}
# A proven-stopped daemon's barrier is removed (status 0). Any other answer,
# including a failed query, may still be a running daemon, which keeps its
# socket name reachable, so the node is restored (status 1). `legacy_state`
# keeps the printed state for the caller's report.
settle_barrier() {
    query_legacy_state || :
    case "$legacy_state" in
        inactive|failed)
            remove_barrier
            return 0
            ;;
        *)
            restore_barrier
            return 1
            ;;
    esac
}
# Settles a barrier still in place when the run ends outside the handled
# paths (an unexpected `set -e` exit or a signal), so the legacy daemon never
# stays reachable only through the moved node. Further signals are ignored
# while the state query runs; the exit keeps the original status, and a
# signal is re-raised with its default action so the caller sees it.
settle_barrier_on_exit() {
    trap '' HUP INT TERM
    if [ -n "$barrier_socket" ] && [ -e "$barrier_socket" ]; then
        settle_barrier || :
    fi
}
barrier_exit_trap() {
    barrier_exit_status=$1
    settle_barrier_on_exit
    exit "$barrier_exit_status"
}
barrier_signal_trap() {
    trap - EXIT
    settle_barrier_on_exit
    trap - "$1"
    kill -s "$1" "$$" || :
    exit "$2"
}
legacy_retired=0
if [ -e "$legacy_unit_dir/pohunekd.service" ]; then
    legacy_socket="${XDG_RUNTIME_DIR:-}/$POHUNEK_RUNTIME_SUBDIR/$POHUNEK_SOCKET_NAME"
    barrier_socket=
    if ! query_legacy_state; then
        echo "could not query the legacy daemon state with" >&2
        echo "\`systemctl --user is-active pohunekd.service\`; nothing was changed" >&2
        echo "fix the reported problem and re-run $0" >&2
        exit 1
    fi
    legacy_stopped=0
    case "$legacy_state" in
        inactive|failed) legacy_stopped=1 ;;
    esac
    if [ "$legacy_stopped" -eq 0 ]; then
        # Move the daemon's listening socket node away with rename(2): its
        # bound socket keeps serving established connections, while any new
        # client cannot connect, so no new session can start after the
        # preflight that follows. A daemon that is not proven stopped but has
        # no socket node (still starting, or already shutting down) cannot be
        # asked whether it owns live PTYs, so the run refuses.
        if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ ! -S "$legacy_socket" ]; then
            echo "the legacy daemon is not stopped (state: $legacy_state), but its control socket is missing:" >&2
            echo "  $legacy_socket" >&2
            echo "nothing was changed; wait until the legacy daemon is active or stopped" >&2
            echo "(or stop it with \`systemctl --user stop pohunekd.service\`) and re-run $0" >&2
            exit 1
        fi
        barrier_socket="$legacy_socket.retiring.$$"
        # Armed before the rename so no interruption lands between the move
        # and the trap; until the moved node exists the traps find nothing to
        # settle. The fallback status of a signal is 128 plus its number.
        trap 'barrier_exit_trap "$?"' EXIT
        trap 'barrier_signal_trap HUP 129' HUP
        trap 'barrier_signal_trap INT 130' INT
        trap 'barrier_signal_trap TERM 143' TERM
        if ! mv "$legacy_socket" "$barrier_socket"; then
            disarm_barrier
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
    # including workers sitting in `activating`. The unit listing is captured
    # before filtering because a POSIX pipeline reports only the last command's
    # status: a failed query must fail the inventory, never read as "no
    # workers".
    legacy_list_workers() {
        legacy_units=$(systemctl --user list-units 'pohunek-session@*' \
            --all --plain --no-legend) || return 1
        printf '%s\n' "$legacy_units" | awk '$3 != "inactive" && NF >= 3'
    }
    if ! before_stop_workers=$(legacy_list_workers); then
        echo "could not list the legacy template workers; nothing was changed" >&2
        echo "fix the reported \`systemctl --user list-units\` problem and re-run $0" >&2
        restore_barrier
        exit 1
    fi
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
    disable_status=0
    systemctl --user disable --now pohunekd.service || disable_status=$?
    if [ "$disable_status" -ne 0 ]; then
        echo "\`systemctl --user disable --now pohunekd.service\` failed (status $disable_status);" >&2
        echo "the legacy unit files were kept:" >&2
        echo "  $legacy_unit_dir/pohunekd.service" >&2
        echo "  $legacy_unit_dir/pohunek-session@.service" >&2
        echo "  $legacy_unit_dir/pohunek-sessions.slice" >&2
        if settle_barrier; then
            echo "the legacy daemon is stopped; fix the reported problem and re-run $0;" >&2
            echo "to keep using the legacy install instead, start it with" >&2
            echo "\`systemctl --user enable --now pohunekd.service\`" >&2
        else
            echo "the legacy daemon may still be running (state: ${legacy_state:-unknown})" >&2
            echo "and stays reachable at $legacy_socket;" >&2
            echo "fix the reported problem and re-run $0" >&2
        fi
        exit "$disable_status"
    fi
    remove_barrier
    # The daemon is stopped and disabled, so its socket is closed and no
    # client can start a session anymore. Re-check the workers it could have
    # spawned since the preflight; anything found here was created inside
    # that window. `--accept-runtime-loss` does not cover it: like the
    # inventory before the stop, a live template worker always blocks the
    # removal of the unit files it runs from.
    # A failed re-check cannot rule such a worker out, so it keeps the unit
    # files like a found one; the stopped daemon's moved socket node is stale
    # and stays removed.
    if ! after_stop_workers=$(legacy_list_workers); then
        echo "the legacy daemon is stopped and disabled, but the template workers" >&2
        echo "could not be listed, so a worker that appeared after the preflight" >&2
        echo "cannot be ruled out; the legacy unit files were kept:" >&2
        echo "  $legacy_unit_dir/pohunekd.service" >&2
        echo "  $legacy_unit_dir/pohunek-session@.service" >&2
        echo "  $legacy_unit_dir/pohunek-sessions.slice" >&2
        echo "fix the reported \`systemctl --user list-units\` problem and re-run $0;" >&2
        echo "to keep using the legacy install instead, start it with" >&2
        echo "\`systemctl --user enable --now pohunekd.service\`" >&2
        exit 1
    fi
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
