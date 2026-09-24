#!/bin/sh
# Install or upgrade the pohunek daemon service from this release archive.
#
# `pohunek service install|upgrade` does the work: versioned binaries under
# <prefix>/libexec/pohunek/<version>/, service.toml, the daemon job, and the
# readiness check. This wrapper only locates the staged binaries next to it and
# retires a pre-service install (a `pohunekd.service` unit with template
# workers) after the one-time migration preflight.

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

# A pre-service install runs `pohunekd.service` from <prefix>/bin with
# `pohunek-session@.service` template workers. Its daemon holds the instance
# locks the service daemon needs, so it is retired first. The archive's own CLI
# runs the preflight, which refuses while the legacy daemon owns live PTYs.
if [ -e "$legacy_unit_dir/pohunekd.service" ] || [ -e "$prefix/bin/pohunekd" ]; then
    if [ "$accept_runtime_loss" -eq 1 ]; then
        "$archive_dir/pohunek" migration preflight --accept-runtime-loss
    else
        "$archive_dir/pohunek" migration preflight
    fi
    if [ -e "$legacy_unit_dir/pohunekd.service" ]; then
        live_workers=$(systemctl --user list-units 'pohunek-session@*' \
            --state=active --plain --no-legend)
        if [ -n "$live_workers" ]; then
            echo "legacy template workers are still running; stop their sessions first:" >&2
            echo "$live_workers" >&2
            exit 1
        fi
        systemctl --user disable --now pohunekd.service
        rm -f \
            "$legacy_unit_dir/pohunekd.service" \
            "$legacy_unit_dir/pohunek-session@.service" \
            "$legacy_unit_dir/pohunek-sessions.slice"
        systemctl --user daemon-reload
    fi
    rm -f "$prefix/bin/pohunekd" "$prefix/libexec/pohunek-sessiond"
fi

if [ -f "$service_config" ]; then
    exec "$archive_dir/pohunek" service upgrade --from "$archive_dir"
fi
exec "$archive_dir/pohunek" service install --from "$archive_dir" --prefix "$prefix"
