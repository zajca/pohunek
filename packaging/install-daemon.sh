#!/bin/sh
# Install or upgrade the pohunek daemon service from this release archive.
#
# `pohunek service install|upgrade` does the work: versioned binaries under
# <prefix>/libexec/pohunek/<version>/, service.toml, the daemon job, and the
# readiness check. This wrapper only locates the staged binaries next to it,
# refuses a pre-service install (a `pohunekd.service` unit with template
# workers, or a pending `migrations/durable-session-workers.json` manifest)
# and picks the subcommand:
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
# `POHUNEK_SERVICE_LOCK_TOKEN` carries. Before the final command,
# `pohunek service check` runs every check it makes before its first effect
# (HOME and the XDG roots, the prefix, every directory it writes, a pending
# transaction, the recorded installation), so a host the final command would
# refuse is never changed.
#
# The archive check (`packaging/verify-archive`) runs `id` from a fixed list of
# system directories, never from PATH: /usr/bin, /bin and, on NixOS, the system
# profile /run/current-system/sw/bin. A host with none of them is refused with
# `no trusted id found` before anything is changed.
#
# After a successful install or upgrade the wrapper prints the exact commands
# that bring the managed agent hook assets in line with the new release; the
# daemon itself never rewrites files in an agent's config home.
#
# `--accept-runtime-loss` is the single flag that accepts live sessions losing
# their runtime. For a service-managed upgrade it is passed to both `service check`
# and `service upgrade`, whose upgrade preflight refuses (with the typed error
# `service_upgrade_sessions_at_risk`) while a live session would not be adopted
# by the new daemon or would lose its native recovery; the flag lets the
# upgrade proceed and report what it accepted; the completed upgrade records the
# accepted verdicts, so the identical risk is not asked about again at the next
# upgrade, and no session is changed. A store the new daemon refuses
# to start with is never overridable. `service install` takes no such flag and
# is never given it.

set -eu

usage() {
    echo "usage: $0 [--accept-runtime-loss]" >&2
    echo "  --accept-runtime-loss  accept live sessions the upgraded daemon would not adopt or" >&2
    echo "                         would lose native recovery for; a store the new daemon" >&2
    echo "                         refuses is never accepted" >&2
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

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
archive_dir=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
config_home=${XDG_CONFIG_HOME:-"$HOME/.config"}
service_config="$config_home/pohunek/service.toml"
legacy_unit_dir="$config_home/systemd/user"

# Everything below that runs a binary from the archive, or changes the host,
# comes after this check: the archive is the daemon archive of one version
# built for this host, complete, unmodified, and not writable by another
# account.
sh "$archive_dir/packaging/verify-archive" "$archive_dir" daemon pohunek pohunekd pohunek-sessiond || exit 1
# A pre-service install (a `pohunekd.service` unit with `pohunek-session@`
# template workers) is refused: release v0.40.1 is the last one that migrates
# it. Only the Linux installer ever wrote that unit, so on another host the
# file is a stray one to remove.
if [ -e "$legacy_unit_dir/pohunekd.service" ]; then
    echo "$legacy_unit_dir/pohunekd.service is a pre-service pohunek install;" >&2
    echo "nothing was changed" >&2
    if [ "$(uname -s)" = Linux ]; then
        echo "install release v0.40.1 first (it migrates the host to the service-managed daemon)," >&2
        echo "then re-run $0 from this release" >&2
    else
        echo "this host never ran that install: remove the file and re-run $0" >&2
    fi
    exit 1
fi

# The v0.40.1 migration removes the units before its final install, so a run
# that stopped there leaves its manifest unimported. Release v0.40.1 imports it
# while no session record exists; once a record is written the sessions are
# lost, so this release is refused while the manifest is pending.
legacy_manifest="${XDG_DATA_HOME:-"$HOME/.local/share"}/pohunek/migrations/durable-session-workers.json"
if [ -e "$legacy_manifest" ]; then
    echo "$legacy_manifest is a pending migration manifest of a pre-service install;" >&2
    echo "nothing was changed" >&2
    echo "install release v0.40.1 first (its daemon imports the manifest)," >&2
    echo "then re-run $0 from this release" >&2
    exit 1
fi

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
    exec "$archive_dir/pohunek" service lock -- sh "$script_dir/install-daemon.sh" "$@"
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
# because the final command writes below it.
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
# Every check the final command makes before its first effect runs first,
# through the same Rust code: `HOME` and the XDG roots the daemon needs, the
# prefix and every directory the command writes (the prefix, `<prefix>/bin`,
# `<prefix>/libexec`, the versions directory, the user unit directory, the
# directory of service.toml, and the private state and runtime roots), a
# pending transaction it would refuse, and the recorded installation of an
# upgrade. The check runs under this run's lock, which its
# report confirms, so its answer holds until the final command.
set -- service check --prefix "$prefix" --json
if [ "$service_action" = upgrade ] && [ "$accept_runtime_loss" -eq 1 ]; then
    set -- "$@" --accept-runtime-loss
fi
if ! check_json=$("$archive_dir/pohunek" "$@"); then
    printf '%s\n' "$check_json" >&2
    echo "\`pohunek service $service_action\` would refuse this host;" >&2
    echo "nothing was changed; fix the reported problem and re-run $0" >&2
    if printf '%s\n' "$check_json" | grep -q 'service_upgrade_sessions_at_risk'; then
        echo "live sessions the upgrade would not carry over are listed above" >&2
        if printf '%s\n' "$check_json" | grep -q 'the path a pending search-path refresh'; then
            echo "a search-path refresh is pending and the refusal judged with the path its rollback restores:" >&2
            echo "run the same pohunek that started the refresh with: service refresh-path (without a dry run) to finish it, then re-run $0 without --accept-runtime-loss" >&2
        elif printf '%s\n' "$check_json" | grep -q 'the search path this upgrade judged with is:'; then
            echo "the refusal names the recorded search path; if a runtime moved since it was recorded, do not accept the loss:" >&2
            echo "run $archive_dir/pohunek service refresh-path --dry-run, then $archive_dir/pohunek service refresh-path, then re-run $0 without --accept-runtime-loss" >&2
            echo "otherwise re-run $0 --accept-runtime-loss to accept losing the listed sessions" >&2
        else
            echo "re-run $0 --accept-runtime-loss to accept losing them" >&2
        fi
    fi
    exit 1
fi
if ! printf '%s\n' "$check_json" | grep -Eq '"locked"[[:space:]]*:[[:space:]]*true'; then
    printf '%s\n' "$check_json" >&2
    echo "\`pohunek service check\` did not run under this run's transaction lock; nothing was changed" >&2
    exit 1
fi

if [ "$service_action" = upgrade ]; then
    set -- service upgrade --from "$archive_dir"
    if [ "$accept_runtime_loss" -eq 1 ]; then
        set -- "$@" --accept-runtime-loss
    fi
else
    set -- service install --from "$archive_dir" --prefix "$prefix"
fi
status=0
"$archive_dir/pohunek" "$@" || status=$?
# The daemon never rewrites the hook assets in the agents' config homes. The
# new daemon is running by now, so its doctor compares the installed assets with
# the ones this release embeds.
if [ "$status" -eq 0 ]; then
    echo "next: check the agent hook assets against this release with" >&2
    echo "\`pohunek integration doctor\`, and reinstall each Claude or Codex agent it" >&2
    echo "reports as outdated with \`pohunek integration install --agent <agent>\`" >&2
    echo "(add \`--profile <name>\` for a host profile's home)" >&2
    echo "for an outdated Hermes plugin run \`pohunek integration update --agent hermes\`" >&2
    echo "with the \`--hermes-profile\` or \`--hermes-home\` it was installed with" >&2
fi
exit "$status"
