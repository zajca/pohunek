#!/usr/bin/env sh
# Install or update the Pohunek web control center backend for the current user.
#
# usage: ./install.sh [--uninstall]
#
# Linux writes a systemd user unit; the operator enables it. macOS registers a
# launchd login agent for this installation (see the macOS notes below).
# `--uninstall` is macOS-only: it removes the backend's agent and installed
# files and keeps `backend.env` and the logs.
#
# macOS notes. The backend is a separate client service: it is registered under
# its own label `io.github.zajca.pohunek.<namespace>.backend`, where the
# namespace is the one of the installed daemon (`pohunek service status`), and
# installing, updating, or removing it never touches the daemon or any session.
# Its property list is written with plutil, so no path or value is interpolated
# into XML. The job environment holds only the non-secret POHUNEK_BACKEND_*
# settings copied from `backend.env` (an allowlist; anything else in the file
# is refused), the installer-managed static directory and log directory, and
# XDG_RUNTIME_DIR when it is set. launchd has no environment file, so the
# agent is registered only once BIND_HOST and PORT are set, and re-running the
# installer after editing `backend.env` re-renders it.

set -eu

readonly_archive_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

action=install
if [ "${1:-}" = "--uninstall" ]; then
  action=uninstall
  shift
fi
if [ "$#" -ne 0 ]; then
  printf '%s\n' "usage: $0 [--uninstall]" >&2
  exit 2
fi

case $(uname -s) in
  Linux) platform=linux ;;
  Darwin) platform=macos ;;
  *)
    printf '%s\n' "unsupported host: $(uname -s) (supported: Linux x86_64, macOS arm64)" >&2
    exit 1
    ;;
esac
if [ "$action" = uninstall ] && [ "$platform" != macos ]; then
  printf '%s\n' "--uninstall is available on macOS only; on Linux run: systemctl --user disable --now pohunek-backend.service" >&2
  exit 2
fi

readonly_data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
readonly_config_home=${XDG_CONFIG_HOME:-"$HOME/.config"}
readonly_state_home=${XDG_STATE_HOME:-"$HOME/.local/state"}
readonly_install_dir="$readonly_data_home/pohunek/web"
readonly_config_dir="$readonly_config_home/pohunek"
readonly_config_file="$readonly_config_dir/backend.env"
readonly_unit_dir="$readonly_config_home/systemd/user"
readonly_unit_file="$readonly_unit_dir/pohunek-backend.service"
readonly_install_parent="$readonly_data_home/pohunek"
readonly_log_dir="$readonly_state_home/pohunek/web-logs"
readonly_agent_dir="$HOME/Library/LaunchAgents"

# Settings `backend.env` may carry into the launchd job. The static and log
# directories belong to the installer.
allowed_setting() {
  case $1 in
    POHUNEK_BACKEND_BIND_HOST | POHUNEK_BACKEND_PORT | POHUNEK_BACKEND_ALLOW_LOOPBACK | \
      POHUNEK_BACKEND_DAEMON_SOCKET | POHUNEK_BACKEND_DISCOVER_INTERVAL | \
      POHUNEK_BACKEND_LOG_MAX_FILE_BYTES | POHUNEK_BACKEND_LOG_MAX_FILES) return 0 ;;
    *) return 1 ;;
  esac
}

# Sets `agent_label` and `agent_file` from the installed daemon's namespace.
# The label namespace keeps this agent apart from any other installation's.
resolve_agent() {
  pohunek_bin=${POHUNEK_BIN:-"$HOME/.local/bin/pohunek"}
  if [ ! -x "$pohunek_bin" ]; then
    printf '%s\n' "the pohunek CLI is not at $pohunek_bin; install the daemon archive first, or set POHUNEK_BIN" >&2
    exit 1
  fi
  if ! status_json=$("$pohunek_bin" service status --json); then
    printf '%s\n' "$status_json" >&2
    printf '%s\n' "\`pohunek service status --json\` failed; nothing was changed" >&2
    exit 1
  fi
  if ! printf '%s\n' "$status_json" | grep -Eq '"installed"[[:space:]]*:[[:space:]]*true'; then
    printf '%s\n' "the daemon is not installed; run packaging/install-daemon.sh from the daemon archive first" >&2
    exit 1
  fi
  if ! namespace_line=$(printf '%s\n' "$status_json" | awk '
      /^[[:space:]]*"namespace"[[:space:]]*:/ { count++; line = $0 }
      END { if (count != 1) exit 1; print line }'); then
    printf '%s\n' "\`pohunek service status --json\` did not report exactly one namespace" >&2
    exit 1
  fi
  namespace=${namespace_line#*\"namespace\"}
  namespace=${namespace#*:}
  namespace=${namespace#"${namespace%%[![:space:]]*}"}
  namespace=${namespace%,}
  case $namespace in
    \"*\") namespace=${namespace#\"}; namespace=${namespace%\"} ;;
    *) printf '%s\n' "the daemon reports no namespace" >&2; exit 1 ;;
  esac
  case $namespace in
    '' | *[!a-z0-9]*) printf '%s\n' "unexpected namespace: $namespace" >&2; exit 1 ;;
  esac
  agent_label="io.github.zajca.pohunek.$namespace.backend"
  agent_file="$readonly_agent_dir/$agent_label.plist"
}

domain="gui/$(id -u)"

agent_loaded() {
  launchctl print "$domain/$agent_label" >/dev/null 2>&1
}

# Boots the agent out of the login domain and waits until launchd forgets it.
unload_agent() {
  if ! agent_loaded; then
    return 0
  fi
  launchctl bootout "$domain/$agent_label"
  waited=0
  while agent_loaded; do
    waited=$((waited + 1))
    if [ "$waited" -gt 60 ]; then
      printf '%s\n' "launchd still has $agent_label loaded after 60 seconds" >&2
      exit 1
    fi
    sleep 1
  done
}

if [ "$action" = uninstall ]; then
  resolve_agent
  unload_agent
  rm -f -- "$agent_file"
  rm -rf -- "$readonly_install_dir"
  printf '%s\n' "Removed the web control center agent $agent_label and $readonly_install_dir."
  printf '%s\n' "Kept $readonly_config_file and $readonly_log_dir; the daemon and every session are untouched."
  exit 0
fi

# Nothing is read from the archive, run, or changed before this check passes.
if [ -f "$readonly_archive_dir/packaging/verify-archive" ]; then
  sh "$readonly_archive_dir/packaging/verify-archive" "$readonly_archive_dir" web pohunek-web || exit 1
else
  printf '%s\n' "packaging/verify-archive is missing in $readonly_archive_dir; nothing was changed" >&2
  exit 1
fi

if [ ! -x "$readonly_archive_dir/pohunek-web" ]; then
  printf '%s\n' "pohunek-web is missing or not executable in $readonly_archive_dir" >&2
  exit 1
fi

if [ "$platform" = linux ] && [ ! -f "$readonly_archive_dir/pohunek-backend.service.in" ]; then
  printf '%s\n' "pohunek-backend.service.in is missing in $readonly_archive_dir" >&2
  exit 1
fi

if [ ! -d "$readonly_archive_dir/frontend" ]; then
  printf '%s\n' "frontend assets are missing in $readonly_archive_dir" >&2
  exit 1
fi

if [ ! -f "$readonly_archive_dir/backend.env.example" ]; then
  printf '%s\n' "backend.env.example is missing in $readonly_archive_dir" >&2
  exit 1
fi

if [ "$platform" = macos ]; then
  # Resolved before anything changes: no daemon, no namespace, no install.
  resolve_agent
  mkdir -p "$readonly_install_parent" "$readonly_config_dir" "$readonly_agent_dir"
else
  mkdir -p "$readonly_install_parent" "$readonly_config_dir" "$readonly_unit_dir"
fi

staging_dir=
backup_dir=
unit_temp=
lock_dir=

cleanup() {
  if [ -n "$unit_temp" ] && [ -e "$unit_temp" ]; then
    rm -f -- "$unit_temp"
  fi
  if [ -n "$staging_dir" ] && [ -d "$staging_dir" ]; then
    rm -rf -- "$staging_dir"
  fi
  if [ -n "$lock_dir" ]; then
    rm -rf -- "$lock_dir"
  fi
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

# One installer at a time: the install directory is swapped and the agent
# re-registered, which two runs must not interleave. A lock left by a killed
# run is taken over when its process is gone.
lock_path="$readonly_install_parent/.web-install.lock"
if ! mkdir "$lock_path" 2>/dev/null; then
  holder=$(cat "$lock_path/pid" 2>/dev/null || true)
  if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
    printf '%s\n' "another web installer is running (pid $holder); nothing was changed" >&2
    exit 1
  fi
  rm -rf -- "$lock_path"
  if ! mkdir "$lock_path" 2>/dev/null; then
    printf '%s\n' "another web installer is running; nothing was changed" >&2
    exit 1
  fi
fi
lock_dir=$lock_path
printf '%s\n' "$$" > "$lock_dir/pid"

staging_dir=$(mktemp -d "$readonly_install_parent/.web-install.XXXXXX")

mkdir -p "$staging_dir/frontend"
install -m 0755 "$readonly_archive_dir/pohunek-web" "$staging_dir/pohunek-web"
cp -R "$readonly_archive_dir/frontend/." "$staging_dir/frontend/"

if [ ! -f "$readonly_config_file" ]; then
  install -m 0600 "$readonly_archive_dir/backend.env.example" "$readonly_config_file"
  printf '%s\n' "Created $readonly_config_file; set POHUNEK_BACKEND_BIND_HOST and POHUNEK_BACKEND_PORT before starting the service."
else
  chmod 0600 "$readonly_config_file"
fi

escape_systemd_value() {
  sed \
    -e 's/\\/\\\\/g' \
    -e 's/"/\\"/g' \
    -e 's/%/%%/g' \
    -e 's/\$/$$/g'
}

escape_systemd_file_path() {
  sed \
    -e 's/\\/\\\\/g' \
    -e 's/ /\\x20/g' \
    -e 's/%/%%/g'
}

escape_sed_replacement() {
  sed 's/[&|\\]/\\&/g'
}

# Writes the launchd property list for the backend to $1. Settings come from
# `backend.env` after validation; `have_listener` is set to yes when both the
# bind host and the port are present.
render_agent() {
  have_host=no
  have_port=no
  seen=" "
  settings=$(mktemp "$readonly_install_parent/.web-settings.XXXXXX")
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in
      '' | '#'*) continue ;;
    esac
    key=${line%%=*}
    if [ "$key" = "$line" ]; then
      rm -f -- "$settings"
      printf '%s\n' "$readonly_config_file: line without '=': $line" >&2
      exit 1
    fi
    value=${line#*=}
    case $key in
      POHUNEK_BACKEND_STATIC_DIR | POHUNEK_BACKEND_LOG_DIR)
        rm -f -- "$settings"
        printf '%s\n' "$readonly_config_file: $key is managed by the installer on macOS; remove it" >&2
        exit 1
        ;;
    esac
    if ! allowed_setting "$key"; then
      rm -f -- "$settings"
      printf '%s\n' "$readonly_config_file: $key is not a supported backend setting" >&2
      exit 1
    fi
    case $seen in
      *" $key "*)
        rm -f -- "$settings"
        printf '%s\n' "$readonly_config_file: $key is set twice" >&2
        exit 1
        ;;
    esac
    seen="$seen$key "
    case $value in
      \"*\") value=${value#\"}; value=${value%\"} ;;
      \'*\') value=${value#\'}; value=${value%\'} ;;
    esac
    [ -n "$value" ] || continue
    [ "$key" != POHUNEK_BACKEND_BIND_HOST ] || have_host=yes
    [ "$key" != POHUNEK_BACKEND_PORT ] || have_port=yes
    printf '%s\n' "$key" >> "$settings"
    printf '%s\n' "$value" >> "$settings"
  done < "$readonly_config_file"
  have_listener=no
  if [ "$have_host" = yes ] && [ "$have_port" = yes ]; then
    have_listener=yes
  fi

  plutil -create xml1 "$1"
  plutil -insert Label -string "$agent_label" "$1"
  plutil -insert ProgramArguments -array "$1"
  plutil -insert ProgramArguments.0 -string "$readonly_install_dir/pohunek-web" "$1"
  plutil -insert EnvironmentVariables -dictionary "$1"
  plutil -insert EnvironmentVariables.POHUNEK_BACKEND_STATIC_DIR -string "$readonly_install_dir/frontend" "$1"
  plutil -insert EnvironmentVariables.POHUNEK_BACKEND_LOG_DIR -string "$readonly_log_dir" "$1"
  if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    plutil -insert EnvironmentVariables.XDG_RUNTIME_DIR -string "$XDG_RUNTIME_DIR" "$1"
  fi
  while IFS= read -r key && IFS= read -r value; do
    plutil -insert "EnvironmentVariables.$key" -string "$value" "$1"
  done < "$settings"
  rm -f -- "$settings"
  plutil -insert RunAtLoad -bool true "$1"
  plutil -insert KeepAlive -dictionary "$1"
  plutil -insert KeepAlive.SuccessfulExit -bool false "$1"
  # A configuration error exits at once; the throttle bounds the restart rate.
  plutil -insert ThrottleInterval -integer 30 "$1"
  plutil -insert ProcessType -string Background "$1"
  plutil -insert LimitLoadToSessionType -string Aqua "$1"
  plutil -insert WorkingDirectory -string "$readonly_install_dir" "$1"
  plutil -insert StandardOutPath -string /dev/null "$1"
  plutil -insert StandardErrorPath -string "$readonly_log_dir/launchd.stderr" "$1"
  plutil -lint "$1" > /dev/null
}

if [ "$platform" = macos ]; then
  unit_temp=$(mktemp "$readonly_agent_dir/.pohunek-backend.plist.XXXXXX")
  render_agent "$unit_temp"
  chmod 0644 "$unit_temp"
else
  readonly_escaped_install_dir=$(
    printf '%s' "$readonly_install_dir" | escape_systemd_value | escape_sed_replacement
  )
  readonly_escaped_config_file=$(
    printf '%s' "$readonly_config_file" | escape_systemd_file_path | escape_sed_replacement
  )
  unit_temp=$(mktemp "$readonly_unit_dir/.pohunek-backend.service.XXXXXX")
  sed \
    -e "s|@INSTALL_DIR@|$readonly_escaped_install_dir|g" \
    -e "s|@CONFIG_FILE@|$readonly_escaped_config_file|g" \
    "$readonly_archive_dir/pohunek-backend.service.in" > "$unit_temp"
  chmod 0644 "$unit_temp"
fi

if [ -e "$readonly_install_dir" ] || [ -L "$readonly_install_dir" ]; then
  backup_dir=$(mktemp -d "$readonly_install_parent/.web-backup.XXXXXX")
  rmdir "$backup_dir"
  mv "$readonly_install_dir" "$backup_dir"
fi

if ! mv "$staging_dir" "$readonly_install_dir"; then
  if [ -n "$backup_dir" ] && [ -d "$backup_dir" ]; then
    mv "$backup_dir" "$readonly_install_dir"
    backup_dir=
  fi
  exit 1
fi
staging_dir=

if [ "$platform" = macos ]; then
  mv "$unit_temp" "$agent_file"
  unit_temp=
else
  mv "$unit_temp" "$readonly_unit_file"
  unit_temp=
fi

if [ -n "$backup_dir" ] && [ -d "$backup_dir" ]; then
  rm -rf -- "$backup_dir"
  backup_dir=
fi

printf '%s\n' "Installed Pohunek web control center to $readonly_install_dir"

if [ "$platform" = macos ]; then
  if [ "$have_listener" = yes ]; then
    mkdir -p "$readonly_log_dir"
    chmod 0700 "$readonly_log_dir"
    unload_agent
    launchctl bootstrap "$domain" "$agent_file"
    printf '%s\n' "Registered and started $agent_label; the daemon and every session were not touched."
    printf '%s\n' "After editing $readonly_config_file, run ./install.sh again to apply it."
  else
    printf '%s\n' "Set POHUNEK_BACKEND_BIND_HOST and POHUNEK_BACKEND_PORT in $readonly_config_file, then run ./install.sh again to register the agent."
  fi
else
  printf '%s\n' "After configuring $readonly_config_file, run:"
  printf '%s\n' "  systemctl --user daemon-reload"
  printf '%s\n' "  systemctl --user enable --now pohunek-backend.service"
fi
