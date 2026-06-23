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

pohunek_template_path() {
  name="$1"
  path="$(pohunek_config_dir)/prompts/$name.tmpl"
  [ -f "$path" ] || pohunek_fail "missing prompt template: $path"
  printf '%s\n' "$path"
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
  pohunek_need_cmd python3
  template="$1"
  provider="$2"
  item_id="$3"
  json="$4"
  python3 - "$template" "$provider" "$item_id" "$json" <<'PY'
import json
import re
import sys

template_path, provider, item_id, raw_json = sys.argv[1:5]
try:
    data = json.loads(raw_json)
except json.JSONDecodeError as exc:
    raise SystemExit(f"provider returned invalid JSON: {exc}")

def pick(*names, required=False):
    for name in names:
        value = data.get(name)
        if isinstance(value, str) and value:
            return value
    if required:
        raise SystemExit(f"provider JSON missing required field: {'/'.join(names)}")
    return ""

if provider == "github_pr":
    context = {
        "provider": "github",
        "number": item_id,
        "id": item_id,
        "title": pick("title", required=True),
        "body": pick("body", "description"),
        "branch": pick("headRefName", "branch", "branchName", required=True),
        "url": pick("url"),
    }
elif provider == "linear_issue":
    context = {
        "provider": "linear",
        "id": pick("identifier", "id") or item_id,
        "number": pick("identifier", "id") or item_id,
        "title": pick("title", required=True),
        "body": pick("description", "body"),
        "branch": pick("branchName", "branch", required=True),
        "url": pick("url"),
    }
else:
    raise SystemExit(f"unknown provider: {provider}")

with open(template_path, encoding="utf-8") as handle:
    template = handle.read()

unknown = sorted(set(re.findall(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}", template)) - set(context))
if unknown:
    raise SystemExit(f"template references unknown variable(s): {', '.join(unknown)}")

# Single pass: substitute every ${var} once, so a provider-controlled value
# (issue/PR title or body) that itself contains a literal ${other} is never
# re-expanded by a later substitution. The unknown-var check above guarantees
# every matched key is present in context.
rendered = re.sub(
    r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}",
    lambda match: context[match.group(1)],
    template,
)
sys.stdout.write(rendered)
PY
}

pohunek_run_session_new() {
  pohunek_bin="$1"
  host="$2"
  agent="$3"
  repo="$4"
  branch="$5"
  prompt="$6"
  yes="$7"

  if [ -n "$host" ]; then
    set -- "$pohunek_bin" --host "$host" session new
  else
    set -- "$pohunek_bin" session new
  fi
  set -- "$@" --agent "$agent" --repo "$repo" --branch "$branch" --input "$prompt"
  if [ "$yes" = "true" ]; then
    set -- "$@" --yes
  fi
  exec "$@"
}
