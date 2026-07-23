#!/usr/bin/env sh

set -eu

readonly_archive_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
readonly_data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
readonly_config_home=${XDG_CONFIG_HOME:-"$HOME/.config"}
readonly_install_dir="$readonly_data_home/pohunek/web"
readonly_config_dir="$readonly_config_home/pohunek"
readonly_config_file="$readonly_config_dir/backend.env"
readonly_unit_dir="$readonly_config_home/systemd/user"
readonly_unit_file="$readonly_unit_dir/pohunek-backend.service"
readonly_install_parent="$readonly_data_home/pohunek"

if [ ! -x "$readonly_archive_dir/pohunek-web" ]; then
  printf '%s\n' "pohunek-web is missing or not executable in $readonly_archive_dir" >&2
  exit 1
fi

if [ ! -f "$readonly_archive_dir/pohunek-backend.service.in" ]; then
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

mkdir -p "$readonly_install_parent" "$readonly_config_dir" "$readonly_unit_dir"

staging_dir=$(mktemp -d "$readonly_install_parent/.web-install.XXXXXX")
backup_dir=
unit_temp=

cleanup() {
  if [ -n "$unit_temp" ] && [ -e "$unit_temp" ]; then
    rm -f -- "$unit_temp"
  fi
  if [ -n "$staging_dir" ] && [ -d "$staging_dir" ]; then
    rm -rf -- "$staging_dir"
  fi
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

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

escape_sed_replacement() {
  sed 's/[&|\\]/\\&/g'
}

readonly_escaped_install_dir=$(
  printf '%s' "$readonly_install_dir" | escape_systemd_value | escape_sed_replacement
)
readonly_escaped_config_file=$(
  printf '%s' "$readonly_config_file" | escape_systemd_value | escape_sed_replacement
)
unit_temp=$(mktemp "$readonly_unit_dir/.pohunek-backend.service.XXXXXX")
sed \
  -e "s|@INSTALL_DIR@|$readonly_escaped_install_dir|g" \
  -e "s|@CONFIG_FILE@|$readonly_escaped_config_file|g" \
  "$readonly_archive_dir/pohunek-backend.service.in" > "$unit_temp"
chmod 0644 "$unit_temp"

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

mv "$unit_temp" "$readonly_unit_file"
unit_temp=

if [ -n "$backup_dir" ] && [ -d "$backup_dir" ]; then
  rm -rf -- "$backup_dir"
  backup_dir=
fi

printf '%s\n' "Installed Pohunek web control center to $readonly_install_dir"
printf '%s\n' "After configuring $readonly_config_file, run:"
printf '%s\n' "  systemctl --user daemon-reload"
printf '%s\n' "  systemctl --user enable --now pohunek-backend.service"
