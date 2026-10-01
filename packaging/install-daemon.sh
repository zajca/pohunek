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
#
# The prefix is `POHUNEK_INSTALL_PREFIX` or `$HOME/.local` for an install and
# the prefix recorded in service.toml for an upgrade, which refuses a
# different `POHUNEK_INSTALL_PREFIX`.
#
# The whole run holds the service transaction lock: the wrapper re-executes
# itself under `pohunek service lock`, so no other `pohunek service
# install|upgrade|uninstall` can start between its first query and the final
# command, and the final command adopts the same lock with the holder token
# `POHUNEK_SERVICE_LOCK_TOKEN` carries. Before the legacy install is
# touched, `pohunek service check` runs every check the final command makes
# before its first effect (HOME and the XDG roots, the prefix, every directory
# it writes, a pending transaction, the recorded installation), so a host the
# final command would refuse never has its legacy install retired.

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
config_home=${XDG_CONFIG_HOME:-"$HOME/.config"}
service_config="$config_home/pohunek/service.toml"
legacy_unit_dir="$config_home/systemd/user"

# Application subdirectory below each XDG root (the runtime directory among
# them) and control-socket file name the daemon binds, per the shared path
# contract in crates/paths (`APP_DIR` and `SOCKET_NAME`); changing either must
# stay in step with that crate.
POHUNEK_APP_DIR=pohunek
POHUNEK_SOCKET_NAME=daemon.sock
# Sibling name the legacy socket node is renamed to as the connect barrier.
# It is no longer than POHUNEK_SOCKET_NAME, so a socket path that fits
# `sun_path` still fits after the rename and the preflight can dial it.
POHUNEK_BARRIER_NAME=retiring

# Everything below that runs a binary from the archive, or changes the host,
# comes after this check: the host is supported, and the archive is the daemon
# archive of one version built for this host, with every member present,
# unmodified, and not writable by another account. The check reads the archive
# MANIFEST (`packaging/write-manifest`) and nothing else, so a corrupt or
# foreign archive is refused before any of its code runs.
refuse_archive() {
    echo "$1" >&2
    echo "nothing was changed; fix the archive (download and extract it again) and re-run $0" >&2
    exit 1
}
host_os=$(uname -s)
host_arch=$(uname -m)
case "$host_os $host_arch" in
    "Linux x86_64") host_targets="x86_64-unknown-linux-gnu x86_64-unknown-linux-musl" ;;
    "Darwin arm64") host_targets="aarch64-apple-darwin" ;;
    "Darwin x86_64")
        echo "this is an Intel Mac or a shell translated by Rosetta; pohunek supports native Apple Silicon only" >&2
        echo "nothing was changed" >&2
        exit 1
        ;;
    *)
        echo "unsupported host: $host_os $host_arch (supported: Linux x86_64, macOS arm64)" >&2
        echo "nothing was changed" >&2
        exit 1
        ;;
esac
manifest="$archive_dir/MANIFEST"
if [ ! -f "$manifest" ] || [ -L "$manifest" ]; then
    refuse_archive "the archive has no MANIFEST: $manifest"
fi
# Every directory above the archive must belong to this user or root and be
# closed to other accounts (a sticky shared directory such as /tmp is fine),
# or another account could rename the verified tree away and put its own under
# the same path.
ancestor=$(CDPATH= cd -- "$archive_dir" && pwd -P)
while [ "$ancestor" != / ]; do
    ancestor=$(dirname -- "$ancestor")
    if [ -n "$(find "$ancestor" -prune \( -perm -020 -o -perm -002 \) ! -perm -1000 -print)" ] \
        || [ -n "$(find "$ancestor" -prune ! -user "$(/usr/bin/id -u)" ! -user 0 -print)" ]; then
        refuse_archive "a directory above the archive is writable by another account or owned by another user: $ancestor"
    fi
done
# Every directory and file of the archive must belong to this user or root, be
# free of group and other write permission, and no entry may be a symbolic
# link: another account could otherwise replace a member (the wrapper
# re-executes itself from this tree) between the digest check and its use.
host_uid=$(/usr/bin/id -u)
unsafe_entry=$(find "$archive_dir" \( -perm -020 -o -perm -002 \) -print -o \
    \( ! -user "$host_uid" ! -user 0 \) -print -o -type l -print | head -n 1)
if [ -n "$unsafe_entry" ]; then
    refuse_archive "the archive is writable by another account, owned by another user, or holds a symbolic link: $unsafe_entry"
fi
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sum=$(sha256sum -- "$1") || refuse_archive "cannot compute the SHA-256 of $1"
    elif command -v shasum >/dev/null 2>&1; then
        sum=$(shasum -a 256 -- "$1") || refuse_archive "cannot compute the SHA-256 of $1"
    else
        refuse_archive "neither sha256sum nor shasum is available to verify the archive"
    fi
    printf '%s\n' "${sum%% *}"
}
manifest_component=
manifest_version=
manifest_target=
manifest_signing=
manifest_minimum_macos=
manifest_header=0
manifest_members=
while IFS= read -r manifest_line; do
    if [ "$manifest_header" -eq 0 ]; then
        [ "$manifest_line" = "pohunek-archive-manifest 1" ] \
            || refuse_archive "MANIFEST is not a pohunek archive manifest of format 1"
        manifest_header=1
        continue
    fi
    case $manifest_line in
        "component "?*) manifest_component=${manifest_line#component } ;;
        "version "?*) manifest_version=${manifest_line#version } ;;
        "target "?*) manifest_target=${manifest_line#target } ;;
        "signing "?*) manifest_signing=${manifest_line#signing } ;;
        "minimum-macos "?*) manifest_minimum_macos=${manifest_line#minimum-macos } ;;
        "sha256 "?*)
            manifest_entry=${manifest_line#sha256 }
            member_hash=${manifest_entry%% *}
            member_path=${manifest_entry#* }
            case $member_hash in
                *[!0-9a-f]* | '') refuse_archive "MANIFEST has a malformed digest: $manifest_line" ;;
            esac
            [ "${#member_hash}" -eq 64 ] \
                || refuse_archive "MANIFEST has a malformed digest: $manifest_line"
            case $member_path in
                '' | /* | ../* | */../* | */.. | .. | ./* | */./* | *//* | *[!A-Za-z0-9._+@=,/-]*)
                    refuse_archive "MANIFEST names an unsafe member path: $member_path"
                    ;;
            esac
            if [ ! -f "$archive_dir/$member_path" ] || [ -L "$archive_dir/$member_path" ]; then
                refuse_archive "archive member is missing or not a regular file: $member_path"
            fi
            if [ -n "$(find "$archive_dir/$member_path" -prune \( -perm -020 -o -perm -002 \) -print)" ]; then
                refuse_archive "archive member is writable by another account: $member_path"
            fi
            [ "$(sha256_of "$archive_dir/$member_path")" = "$member_hash" ] \
                || refuse_archive "archive member is corrupt (digest mismatch): $member_path"
            manifest_members="$manifest_members $member_path "
            ;;
        *) refuse_archive "MANIFEST has an unrecognized line: $manifest_line" ;;
    esac
done < "$manifest"
[ "$manifest_header" -eq 1 ] || refuse_archive "MANIFEST is empty"
[ "$manifest_component" = daemon ] \
    || refuse_archive "this is the ${manifest_component:-unknown} archive; packaging/install-daemon.sh installs the daemon archive"
case $manifest_version in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) refuse_archive "MANIFEST has no valid version" ;;
esac
case $manifest_version in
    *[!0-9.]*) refuse_archive "MANIFEST has no valid version" ;;
esac
host_match=0
for host_target in $host_targets; do
    if [ "$manifest_target" = "$host_target" ]; then
        host_match=1
    fi
done
if [ "$host_match" -eq 0 ]; then
    echo "this archive is built for ${manifest_target:-an unknown target}, but this host is $host_os $host_arch" >&2
    echo "nothing was changed; download the archive for $host_targets" >&2
    exit 1
fi
if [ "$host_os" = Darwin ]; then
    [ -n "$manifest_minimum_macos" ] || refuse_archive "MANIFEST has no minimum-macos"
    host_macos=$(sw_vers -productVersion) || refuse_archive "cannot read the macOS version with sw_vers"
    case $host_macos in
        '' | *[!0-9.]*) refuse_archive "unreadable macOS version: $host_macos" ;;
    esac
    if ! awk -v minimum="$manifest_minimum_macos" -v have="$host_macos" 'BEGIN {
        n = split(have, a, ".")
        m = split(minimum, b, ".")
        count = (n > m) ? n : m
        for (i = 1; i <= count; i++) {
            x = (i <= n) ? a[i] + 0 : 0
            y = (i <= m) ? b[i] + 0 : 0
            if (x > y) exit 0
            if (x < y) exit 1
        }
        exit 0
    }'; then
        echo "this archive needs macOS $manifest_minimum_macos or newer, but this host runs macOS $host_macos" >&2
        echo "nothing was changed" >&2
        exit 1
    fi
fi
for required in pohunek pohunekd pohunek-sessiond; do
    case "$manifest_members" in
        *" $required "*) ;;
        *) refuse_archive "MANIFEST does not list the required binary: $required" ;;
    esac
    if [ ! -x "$archive_dir/$required" ]; then
        refuse_archive "required staged binary is not executable: $archive_dir/$required"
    fi
done

# Everything below runs under the transaction lock. `pohunek service lock`
# refuses with `service_transaction_in_progress` while another `pohunek
# service` command holds it, before this script runs again; nothing was
# changed then. Inside the lock the variable carries the holder's token, and
# the `service` commands below fail rather than run unlocked when it proves
# no live holder.
if [ -z "${POHUNEK_SERVICE_LOCK_TOKEN:-}" ]; then
    if [ "$accept_runtime_loss" -eq 1 ]; then
        set -- --accept-runtime-loss
    fi
    exec "$archive_dir/pohunek" service lock -- sh "$0" "$@"
fi

# Asked before anything changes, so a failing query leaves the host untouched.
# The status report is pretty-printed JSON in which only `pending_transaction`
# carries an "operation" key. A `--json` failure prints its error document on
# stdout, so the captured output is forwarded to stderr. Its
# `transaction_in_progress` names this run's own lock and is not consulted.
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

# The install prefix is resolved and validated before anything changes,
# because the legacy retirement below removes files under it.
#
# Sets `normalized_prefix` to $1 with repeated and trailing slashes collapsed
# (the same path to `pohunek service install --prefix`, which requires an
# absolute path). Fails (status 1) for a relative path, a `.` or `..`
# component, or a newline, which the line-based checks below cannot carry.
normalize_prefix() {
    case $1 in
        /*) ;;
        *) return 1 ;;
    esac
    case $1 in
        *"
"*) return 1 ;;
    esac
    normalized_prefix=
    prefix_rest=${1#/}
    while [ -n "$prefix_rest" ]; do
        prefix_component=${prefix_rest%%/*}
        case $prefix_rest in
            */*) prefix_rest=${prefix_rest#*/} ;;
            *) prefix_rest= ;;
        esac
        case $prefix_component in
            '') ;;
            .|..) return 1 ;;
            *) normalized_prefix="$normalized_prefix/$prefix_component" ;;
        esac
    done
    if [ -z "$normalized_prefix" ]; then
        normalized_prefix=/
    fi
}
refuse_prefix() {
    echo "$1" >&2
    echo "nothing was changed; fix the install prefix and re-run $0" >&2
    exit 1
}
requested_prefix=${POHUNEK_INSTALL_PREFIX:-}
if [ -n "$requested_prefix" ]; then
    normalize_prefix "$requested_prefix" \
        || refuse_prefix "POHUNEK_INSTALL_PREFIX must be an absolute path without \`.\` or \`..\` components: $requested_prefix"
    prefix=$normalized_prefix
fi
if [ "$pending_install" -eq 0 ] && [ -f "$service_config" ]; then
    # `service upgrade` keeps the prefix recorded in service.toml, so that
    # recorded prefix is the one every step here acts on. It is the report's
    # only "prefix" key; a value with a JSON escape is refused rather than
    # decoded.
    service_action=upgrade
    if ! configured_line=$(printf '%s\n' "$status_json" | awk '
        /^[[:space:]]*"prefix"[[:space:]]*:/ { count++; line = $0 }
        END { if (count != 1) exit 1; print line }'); then
        refuse_prefix "\`pohunek service status --json\` did not report exactly one configured prefix"
    fi
    configured_value=${configured_line#*\"prefix\"}
    configured_value=${configured_value#*:}
    configured_value=${configured_value#"${configured_value%%[![:space:]]*}"}
    configured_value=${configured_value%,}
    case $configured_value in
        \"*\") configured_prefix=${configured_value#\"}; configured_prefix=${configured_prefix%\"} ;;
        *) refuse_prefix "service.toml exists, but \`pohunek service status --json\` reports no configured prefix ($configured_value)" ;;
    esac
    case $configured_prefix in
        *\\*|*\"*) refuse_prefix "the configured prefix contains a JSON escape this installer does not decode: $configured_value" ;;
    esac
    normalize_prefix "$configured_prefix" \
        || refuse_prefix "the configured prefix is not an absolute path without \`.\` or \`..\` components: $configured_prefix"
    if [ -n "$requested_prefix" ] && [ "$prefix" != "$normalized_prefix" ]; then
        echo "POHUNEK_INSTALL_PREFIX ($prefix) differs from the prefix of the installed service" >&2
        echo "($normalized_prefix); \`pohunek service upgrade\` keeps the installed prefix." >&2
        refuse_prefix "unset POHUNEK_INSTALL_PREFIX (or set it to $normalized_prefix)"
    fi
    prefix=$normalized_prefix
else
    service_action=install
    if [ -z "$requested_prefix" ]; then
        case ${HOME:-} in
            /*) normalize_prefix "$HOME/.local" ;;
            *) false ;;
        esac || refuse_prefix "HOME must be an absolute path when POHUNEK_INSTALL_PREFIX is unset: ${HOME:-<unset>}"
        prefix=$normalized_prefix
    fi
fi
# The legacy installer rendered `ExecStart=<prefix>/bin/pohunekd` into its
# daemon unit, which names the prefix whose binaries belong to that install.
# A unit that names any other daemon path means this run resolved a
# different prefix, so its binaries are never removed from the wrong tree.
if [ -e "$legacy_unit_dir/pohunekd.service" ]; then
    if ! legacy_exec=$(awk '
        index($0, "ExecStart=") == 1 { count++; value = substr($0, 11) }
        END { if (count != 1) exit 1; print value }' \
        "$legacy_unit_dir/pohunekd.service"); then
        refuse_prefix "the legacy $legacy_unit_dir/pohunekd.service has no single ExecStart= line naming its install prefix"
    fi
    legacy_prefix=
    case $legacy_exec in
        */bin/pohunekd) legacy_prefix=${legacy_exec%/bin/pohunekd}/ ;;
    esac
    if ! normalize_prefix "$legacy_prefix" || [ "$normalized_prefix" != "$prefix" ]; then
        echo "the legacy $legacy_unit_dir/pohunekd.service runs" >&2
        echo "  $legacy_exec" >&2
        echo "which is not <prefix>/bin/pohunekd for the install prefix $prefix;" >&2
        refuse_prefix "set POHUNEK_INSTALL_PREFIX to the legacy install's prefix (for an upgrade, the installed service's prefix must match it)"
    fi
fi

# The legacy retirement below cannot be undone by the final command, so every
# check that command makes before its first effect runs first, through the
# same Rust code: `HOME` and the XDG roots the daemon needs, the prefix and
# every directory the command writes (the prefix, `<prefix>/bin`,
# `<prefix>/libexec`, the versions directory, the user unit directory the
# legacy units live in, the directory of service.toml, and the private state
# and runtime roots), a pending transaction it would refuse, and the recorded
# installation of an upgrade. The check runs under this run's lock, which its
# report confirms, so its answer holds until the final command.
if ! check_json=$("$archive_dir/pohunek" service check --prefix "$prefix" --json); then
    printf '%s\n' "$check_json" >&2
    echo "\`pohunek service $service_action\` would refuse this host, so the legacy install was not retired;" >&2
    echo "nothing was changed; fix the reported problem and re-run $0" >&2
    exit 1
fi
if ! printf '%s\n' "$check_json" | grep -Eq '"locked"[[:space:]]*:[[:space:]]*true'; then
    printf '%s\n' "$check_json" >&2
    echo "\`pohunek service check\` did not run under this run's transaction lock; nothing was changed" >&2
    exit 1
fi

# A pre-service install runs `pohunekd.service` from <prefix>/bin with
# `pohunek-session@.service` template workers. Its daemon holds the instance
# locks the service daemon needs, so it is retired first, in this order:
# socket rename as the connect barrier, preflight over the moved socket,
# worker inventory, disable, removal of the exact legacy paths, daemon-reload.
# The legacy binary is already deployed, so the wrapper cannot add a
# daemon-side barrier to it; the connect barrier closes the only open door and
# every later step fails closed without removing legacy files. Every step
# tolerates a previous partial run: a legacy daemon `systemctl` reports
# stopped is started for the barrier and preflight, and removal and disable
# are no-ops for what is already gone.
#
# Residual windows the wrapper cannot close without a legacy-side change:
# a client that already holds a control connection (a long-lived GUI) may
# still issue session requests between the barrier and the daemon stop, and
# no offline scan of the legacy persisted store covers such a stop-only
# session; its template worker is caught by the post-stop inventory instead.
restore_barrier() {
    # The operator can restart the left-in-place legacy install, so the moved
    # socket node goes back where the daemon expects it. The legacy unit has
    # `Restart=on-failure`: a restarted daemon may bind a new socket at that
    # name at any moment, which a clobbering rename would replace, leaving
    # that daemon unreachable. A hard link is the portable no-replace rename:
    # link(2) fails with EEXIST when the name is taken, and the barrier name
    # is unlinked only after the link exists. The existence check keeps `ln`
    # from linking into a directory or through a symlink at that name.
    if [ -n "$barrier_socket" ] && [ -e "$barrier_socket" ]; then
        if [ ! -e "$legacy_socket" ] && [ ! -L "$legacy_socket" ] \
            && ln "$barrier_socket" "$legacy_socket"; then
            rm -f "$barrier_socket" || :
        elif [ -S "$legacy_socket" ] && [ ! -L "$legacy_socket" ]; then
            # A socket appeared at the original name after the move, so the
            # daemon that owned the moved node exited and a restarted one
            # listens there: the moved node is stale.
            rm -f "$barrier_socket" || :
            echo "a restarted legacy daemon bound a new control socket at" >&2
            echo "  $legacy_socket" >&2
            echo "while the connect barrier was in place; it stays reachable there," >&2
            echo "and the stale moved node $barrier_socket was removed" >&2
        else
            echo "could not put the legacy daemon socket back; it remains at" >&2
            echo "  $barrier_socket" >&2
            echo "move it back with \`mv '$barrier_socket' '$legacy_socket'\`" >&2
            echo "once nothing else occupies $legacy_socket" >&2
        fi
    fi
    disarm_barrier
    return_started_legacy
}
# A legacy daemon this run started only for the migration snapshot is stopped
# again whenever the run refuses before retiring it, so a refused run leaves
# it stopped as it found it.
return_started_legacy() {
    if [ "$legacy_started" -eq 1 ]; then
        legacy_started=0
        if systemctl --user stop pohunekd.service; then
            echo "stopped the legacy daemon again; this run started it only for the migration snapshot" >&2
        else
            echo "could not stop the legacy daemon this run started for the migration snapshot;" >&2
            echo "stop it with \`systemctl --user stop pohunekd.service\`" >&2
        fi
    fi
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
# stays reachable only through the moved node, and stops a legacy daemon this
# run started before any barrier existed. Further signals are ignored while
# the state query runs; the exit keeps the original status, and a signal is
# re-raised with its default action so the caller sees it.
settle_barrier_on_exit() {
    trap '' HUP INT TERM
    if [ -n "$barrier_socket" ] && [ -e "$barrier_socket" ]; then
        settle_barrier || :
    else
        return_started_legacy
    fi
}
# Armed before the first step that changes the legacy install; until a moved
# node or a started daemon exists the traps find nothing to settle. The
# fallback status of a signal is 128 plus its number.
arm_barrier_traps() {
    trap 'barrier_exit_trap "$?"' EXIT
    trap 'barrier_signal_trap HUP 129' HUP
    trap 'barrier_signal_trap INT 130' INT
    trap 'barrier_signal_trap TERM 143' TERM
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
legacy_started=0
if [ -e "$legacy_unit_dir/pohunekd.service" ]; then
    legacy_socket="${XDG_RUNTIME_DIR:-}/$POHUNEK_APP_DIR/$POHUNEK_SOCKET_NAME"
    barrier_path="${XDG_RUNTIME_DIR:-}/$POHUNEK_APP_DIR/$POHUNEK_BARRIER_NAME"
    barrier_socket=
    if ! query_legacy_state; then
        echo "could not query the legacy daemon state with" >&2
        echo "\`systemctl --user is-active pohunekd.service\`; nothing was changed" >&2
        echo "fix the reported problem and re-run $0" >&2
        exit 1
    fi
    case "$legacy_state" in
        inactive|failed)
            # The service daemon converts the legacy records on its first
            # start only from the manifest a preflight against the running
            # legacy daemon writes. Retiring a stopped daemon without that
            # snapshot would leave its resume bindings unimported, or leave
            # the stale manifest of an earlier refused preflight to block the
            # first start, so the daemon is started for the snapshot and every
            # refusal below stops it again. The legacy unit is `Type=notify`:
            # the start returns once the daemon reported ready, after it bound
            # its control socket.
            arm_barrier_traps
            legacy_started=1
            start_status=0
            systemctl --user start pohunekd.service || start_status=$?
            if [ "$start_status" -ne 0 ]; then
                echo "the legacy daemon is stopped (state: $legacy_state), and" >&2
                echo "\`systemctl --user start pohunekd.service\` failed (status $start_status);" >&2
                echo "the migration preflight needs it running, so the legacy install was not retired" >&2
                return_started_legacy
                echo "nothing else was changed; fix the legacy daemon so it starts and re-run $0" >&2
                exit 1
            fi
            if ! query_legacy_state || [ "$legacy_state" != active ]; then
                echo "the legacy daemon started for the migration preflight is not active" >&2
                echo "(state: ${legacy_state:-unknown}), so the legacy install was not retired" >&2
                return_started_legacy
                echo "nothing else was changed; fix the legacy daemon so it starts and re-run $0" >&2
                exit 1
            fi
            ;;
    esac
    # Move the daemon's listening socket node away with rename(2): its bound
    # socket keeps serving established connections, while any new client
    # cannot connect, so no new session can start after the preflight that
    # follows. A daemon that is not proven stopped but has no socket node
    # (still starting, or already shutting down) cannot be asked whether it
    # owns live PTYs, so the run refuses.
    if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ ! -S "$legacy_socket" ]; then
        echo "the legacy daemon is not stopped (state: $legacy_state), but its control socket is missing:" >&2
        echo "  $legacy_socket" >&2
        return_started_legacy
        if [ -n "${XDG_RUNTIME_DIR:-}" ] && [ -S "$barrier_path" ]; then
            echo "an interrupted earlier run left it moved to $barrier_path;" >&2
            echo "nothing was changed; move it back with" >&2
            echo "\`mv '$barrier_path' '$legacy_socket'\` and re-run $0" >&2
            exit 1
        fi
        echo "nothing was changed; wait until the legacy daemon is active or stopped" >&2
        echo "(or stop it with \`systemctl --user stop pohunekd.service\`) and re-run $0" >&2
        exit 1
    fi
    # A node left at the barrier name by an interrupted earlier run is never
    # overwritten or reused: rename(2) would replace it, and `mv` would move
    # the socket into a directory of that name.
    if [ -e "$barrier_path" ] || [ -L "$barrier_path" ]; then
        echo "a connect-barrier node from an interrupted earlier run remains:" >&2
        echo "  $barrier_path" >&2
        return_started_legacy
        echo "nothing was changed; the legacy daemon listens at $legacy_socket," >&2
        echo "so that node is stale: remove it and re-run $0" >&2
        exit 1
    fi
    barrier_socket=$barrier_path
    # Armed before the rename so no interruption lands between the move and
    # the trap.
    arm_barrier_traps
    if ! mv "$legacy_socket" "$barrier_socket"; then
        disarm_barrier
        echo "could not move the legacy daemon socket aside; nothing was changed" >&2
        return_started_legacy
        exit 1
    fi
    # The archive's own CLI runs the preflight over the moved socket. It
    # writes the migration manifest and refuses while the legacy daemon owns
    # live PTYs.
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
    legacy_started=0
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
# The legacy installer wrote <prefix>/bin/pohunekd and
# <prefix>/libexec/pohunek-sessiond with `install` as the invoking user, so
# only a regular file owned by this user, reached through no symlink below the
# prefix, is removed as part of that install. Anything else at those paths
# (a symlink, another user's file, a directory) is left in place and named.
retire_legacy_binary() {
    legacy_binary="$prefix/$1/$2"
    if [ ! -e "$legacy_binary" ] && [ ! -L "$legacy_binary" ]; then
        return 0
    fi
    if [ ! -L "$prefix/$1" ] && [ ! -L "$legacy_binary" ] && [ -f "$legacy_binary" ] \
        && [ -n "$(find "$legacy_binary" -prune -user "$(id -u)" -print 2>/dev/null)" ]; then
        rm -f "$legacy_binary"
        legacy_retired=1
    else
        echo "left $legacy_binary in place: it is not a regular file owned by this user" >&2
        echo "below $prefix, so it is not a binary of the legacy install" >&2
    fi
}
# Only the Linux installer ever wrote these paths; on macOS a file there is the
# owner's own (for example a `cargo install --root ~/.local` binary).
if [ "$host_os" = Linux ]; then
    retire_legacy_binary bin pohunekd
    retire_legacy_binary libexec pohunek-sessiond
fi

if [ "$service_action" = upgrade ]; then
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
