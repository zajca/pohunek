#!/bin/sh

set -eu

pohunek_fail() {
  printf 'pohunek: %s\n' "$*" >&2
  exit 1
}

pohunek_need_cmd() {
  command -v "$1" >/dev/null 2>&1 || pohunek_fail "required command not found: $1"
}

pohunek_config_dir() {
  if [ -n "${POHUNEK_CONFIG_DIR:-}" ]; then
    printf '%s\n' "$POHUNEK_CONFIG_DIR"
  elif [ -n "${XDG_CONFIG_HOME:-}" ]; then
    printf '%s/pohunek\n' "$XDG_CONFIG_HOME"
  elif [ -n "${HOME:-}" ]; then
    printf '%s/.config/pohunek\n' "$HOME"
  else
    pohunek_fail "cannot resolve config dir: set POHUNEK_CONFIG_DIR, XDG_CONFIG_HOME, or HOME"
  fi
}

pohunek_config_file() {
  printf '%s/launcher.conf\n' "$(pohunek_config_dir)"
}

pohunek_config_get() {
  pohunek_need_cmd python3
  file="$(pohunek_config_file)"
  key="$1"
  [ -f "$file" ] || return 1
  python3 - "$file" "$key" <<'PY'
import sys

path, wanted = sys.argv[1], sys.argv[2]
value = None
with open(path, encoding="utf-8") as handle:
    for number, raw in enumerate(handle, 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            raise SystemExit(f"invalid config line {number}: expected key=value")
        key, item = line.split("=", 1)
        key = key.strip()
        item = item.strip()
        if key == wanted:
            value = item
if value is None:
    raise SystemExit(1)
sys.stdout.write(value)
PY
}

pohunek_required_config() {
  value="$(pohunek_config_get "$1" 2>/dev/null || true)"
  [ -n "$value" ] || pohunek_fail "missing required config key '$1' in $(pohunek_config_file)"
  printf '%s\n' "$value"
}

pohunek_optional_config() {
  value="$(pohunek_config_get "$1" 2>/dev/null || true)"
  if [ -n "$value" ]; then
    printf '%s\n' "$value"
  else
    printf '%s\n' "$2"
  fi
}

# Resolve a daemon action recipe for `project`/`action` on `host`.
#
# The daemon is the single source of truth for which agent runs and which prompt
# template is used (resolved per project from the named action, Part A); provider
# fetch + rendering stay caller-side with the caller's own credentials (A.4).
#
# Writes the recipe's prompt-template content to the file at $5, and prints four
# lines to stdout: provider, agent, base_branch, branch (optional lines empty when
# omitted). Under `set -e`, a daemon resolution failure (e.g. `prompt_not_found`)
# aborts the launch HERE, before any session is started — no silent fallback.
pohunek_resolve_action() {
  pohunek_need_cmd python3
  _pra_bin="$1"
  _pra_host="$2"
  _pra_project="$3"
  _pra_action="$4"
  _pra_prompt_out="$5"
  if [ -n "$_pra_host" ]; then
    set -- "$_pra_bin" --host "$_pra_host" project action "$_pra_project" "$_pra_action" --json
  else
    set -- "$_pra_bin" project action "$_pra_project" "$_pra_action" --json
  fi
  _pra_json="$("$@")"
  # Data is passed via argv (not stdin): the heredoc already occupies python's
  # stdin to supply the program, so a piped stdin would be discarded.
  python3 - "$_pra_prompt_out" "$_pra_json" <<'PY'
import json
import sys

out_path, raw_json = sys.argv[1], sys.argv[2]
try:
    data = json.loads(raw_json)
except json.JSONDecodeError as exc:
    raise SystemExit(f"daemon returned invalid action recipe JSON: {exc}")
for key in ("provider", "agent"):
    value = data.get(key)
    if not isinstance(value, str) or not value:
        raise SystemExit(f"daemon action recipe missing required field: {key}")
with open(out_path, "w", encoding="utf-8") as handle:
    handle.write(data["prompt_content"])
sys.stdout.write(data["provider"] + "\n")
sys.stdout.write(data["agent"] + "\n")
sys.stdout.write((data.get("base_branch") or "") + "\n")
sys.stdout.write((data.get("branch") or "") + "\n")
PY
}

# Resolve the authenticated user's Linear display name (the value the issue
# picker filters `--assignee` on) from `linear auth whoami`. whoami has no
# machine-readable mode, so parse its labelled output, stripping any ANSI the
# CLI may emit. Prints nothing (exit 0) when the name cannot be found, leaving
# the caller to fail fast or fall back to an explicit config value.
pohunek_linear_assignee() {
  linear_cli="$1"
  pohunek_need_cmd python3
  # Capture into a variable and pass via argv: the program is supplied on stdin
  # via the heredoc, so whoami's output cannot also come through stdin.
  whoami_out="$("$linear_cli" auth whoami 2>/dev/null || true)"
  python3 - "$whoami_out" <<'PY'
import re
import sys

text = re.sub(r"\x1b\[[0-9;]*m", "", sys.argv[1])
for line in text.splitlines():
    key, sep, value = line.partition(":")
    if sep and key.strip().lower() == "display name":
        value = value.strip()
        if value:
            sys.stdout.write(value)
        break
PY
}

pohunek_render_provider_prompt() {
  template="$1"
  provider="$2"
  item_id="$3"
  json="$4"
  pohunek_bin="$(pohunek_optional_config pohunek_bin pohunek)"
  printf '%s' "$json" | "$pohunek_bin" prompt render \
    --provider "$provider" \
    --item-id "$item_id" \
    --template-file "$template"
}

pohunek_link_meta() {
  provider="$1"
  item_id="$2"
  url="$3"
  json="$4"
  pohunek_bin="$(pohunek_optional_config pohunek_bin pohunek)"
  printf '%s' "$json" | "$pohunek_bin" prompt link \
    --provider "$provider" \
    --item-id "$item_id" \
    --url "$url"
}

pohunek_json_url() {
  # Assumes no escaped quote inside the URL value; true for GitHub/Linear API URLs.
  printf '%s' "$1" | sed -n 's/.*"url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1
}

pohunek_run_session_new() {
  pohunek_bin="$1"
  host="$2"
  agent="$3"
  project="$4"
  branch="$5"
  prompt="$6"
  yes="$7"
  base_branch="${8:-}"
  link_meta="${9:-}"

  if [ -n "$host" ]; then
    set -- "$pohunek_bin" --host "$host" session new
  else
    set -- "$pohunek_bin" session new
  fi
  # Reference the project by id|label (resolved on the target host); no filesystem
  # path crosses the wire. --branch makes the daemon cut a worktree off the
  # project's repo for this issue/PR branch.
  set -- "$@" --agent "$agent" --project "$project" --branch "$branch" --input "$prompt"
  # Honor a template-specified base branch; empty means the daemon falls through
  # to the project default / repo HEAD.
  if [ -n "$base_branch" ]; then
    set -- "$@" --base-branch "$base_branch"
  fi
  if [ -n "$link_meta" ]; then
    while IFS= read -r meta_line; do
      [ -n "$meta_line" ] || continue
      set -- "$@" --meta "$meta_line"
    done <<EOF
$link_meta
EOF
  fi
  if [ "$yes" = "true" ]; then
    set -- "$@" --yes
  fi
  exec "$@"
}
