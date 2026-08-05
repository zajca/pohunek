#!/bin/sh
# Install the pohunek daemon and durable worker systemd user units.

set -eu

accept_runtime_loss=0
if [ "${1:-}" = "--accept-runtime-loss" ]; then
    accept_runtime_loss=1
    shift
fi
if [ "$#" -ne 0 ]; then
    echo "usage: $0 [--accept-runtime-loss]" >&2
    exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
archive_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
prefix=${POHUNEK_INSTALL_PREFIX:-"$HOME/.local"}
bin_dir="$prefix/bin"
libexec_dir="$prefix/libexec"
unit_dir="${XDG_CONFIG_HOME:-"$HOME/.config"}/systemd/user"

for required in \
    "$archive_dir/pohunek" \
    "$archive_dir/pohunekd" \
    "$archive_dir/pohunek-sessiond" \
    "$script_dir/systemd/pohunekd.service.in" \
    "$script_dir/systemd/pohunek-session@.service.in" \
    "$script_dir/systemd/pohunek-sessions.slice"
do
    if [ ! -f "$required" ]; then
        echo "required installation asset is missing: $required" >&2
        exit 1
    fi
done

# The first worker-aware upgrade cannot preserve PTYs owned by a legacy daemon.
# Use the CLI shipped in this archive so the preflight command is guaranteed to
# exist even when the installed CLI predates durable workers. A fresh install
# has no legacy daemon to interrogate. The preflight excludes sessions that
# already expose durable runtime bindings, so later worker-aware upgrades remain
# non-destructive while mixed legacy/worker states still fail closed.
if [ -e "$bin_dir/pohunekd" ] || [ -L "$bin_dir/pohunekd" ]; then
    if [ "$accept_runtime_loss" -eq 1 ]; then
        "$archive_dir/pohunek" migration preflight --accept-runtime-loss
    else
        "$archive_dir/pohunek" migration preflight
    fi
fi

unit_staging=$(mktemp -d)
trap 'rm -rf "$unit_staging"' EXIT HUP INT TERM
sed \
    -e "s|@BIN_DIR@|$archive_dir|g" \
    "$script_dir/systemd/pohunekd.service.in" \
    >"$unit_staging/pohunekd.service"
sed \
    -e "s|@LIBEXEC_DIR@|$archive_dir|g" \
    "$script_dir/systemd/pohunek-session@.service.in" \
    >"$unit_staging/pohunek-session@.service"
install -m 0644 \
    "$script_dir/systemd/pohunek-sessions.slice" \
    "$unit_staging/pohunek-sessions.slice"
systemd-analyze verify \
    "$unit_staging/pohunekd.service" \
    "$unit_staging/pohunek-session@.service" \
    "$unit_staging/pohunek-sessions.slice"

sed \
    -e "s|@BIN_DIR@|$bin_dir|g" \
    "$script_dir/systemd/pohunekd.service.in" \
    >"$unit_staging/pohunekd.service"
sed \
    -e "s|@LIBEXEC_DIR@|$libexec_dir|g" \
    "$script_dir/systemd/pohunek-session@.service.in" \
    >"$unit_staging/pohunek-session@.service"

install -d -m 0755 "$bin_dir" "$libexec_dir" "$unit_dir"
install -m 0755 "$archive_dir/pohunek" "$bin_dir/pohunek"
install -m 0755 "$archive_dir/pohunekd" "$bin_dir/pohunekd"
install -m 0755 "$archive_dir/pohunek-sessiond" "$libexec_dir/pohunek-sessiond"
install -m 0644 "$unit_staging/pohunekd.service" "$unit_dir/pohunekd.service"
install -m 0644 \
    "$unit_staging/pohunek-session@.service" \
    "$unit_dir/pohunek-session@.service"
install -m 0644 \
    "$unit_staging/pohunek-sessions.slice" \
    "$unit_dir/pohunek-sessions.slice"

systemctl --user daemon-reload
systemctl --user enable pohunekd.service
systemctl --user restart pohunekd.service
