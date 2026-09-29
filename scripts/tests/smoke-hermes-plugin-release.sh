#!/usr/bin/env bash
set -euo pipefail

script_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
fixture_root="$script_root/scripts/tests/fixtures"
smoke="$script_root/scripts/smoke-hermes-plugin-release"
test_root="$(mktemp -d /var/tmp/pohunek-hermes-smoke-test.XXXXXX)"
unsafe_temp_parent="$test_root/unsafe"
safe_temp_parent="$test_root/safe"
mkdir -p "$unsafe_temp_parent/.git" "$safe_temp_parent"

cleanup() {
  rm -rf -- "$test_root"
}
trap cleanup EXIT

export POHUNEK_SMOKE_AMBIENT_SENTINEL="controlled-ambient-value"
export OPENAI_API_KEY="controlled-not-a-credential"
export HTTPS_PROXY="http://controlled.invalid"
export HERMES_API_KEY="controlled-not-a-credential"

if "$smoke" "$fixture_root/smoke-pohunek-wrong-layout" "$fixture_root/smoke-hermes" \
  --temp-parent-primary "$unsafe_temp_parent" \
  --temp-parent-fallback "$safe_temp_parent" >/dev/null 2>&1; then
  printf '%s\n' 'wrong plugin layout unexpectedly passed' >&2
  exit 1
fi

output="$(
  "$smoke" "$fixture_root/smoke-pohunek" "$fixture_root/smoke-hermes" \
    --temp-parent-primary "$unsafe_temp_parent" \
    --temp-parent-fallback "$safe_temp_parent"
)"
if [[ "$output" != *"Hermes release-plugin smoke passed."* ]]; then
  printf '%s\n' 'controlled release smoke did not report success' >&2
  exit 1
fi
# macOS reaches its temporary directories through symlinks (/tmp, /var/tmp,
# $TMPDIR under /var). A symlinked parent that resolves outside any repository
# is accepted; one that resolves into a repository is refused.
symlink_parent="$test_root/symlinked-parent"
ln -s "$safe_temp_parent" "$symlink_parent"
output="$(
  "$smoke" "$fixture_root/smoke-pohunek" "$fixture_root/smoke-hermes" \
    --temp-parent-primary "$symlink_parent" \
    --temp-parent-fallback "$unsafe_temp_parent"
)"
if [[ "$output" != *"Hermes release-plugin smoke passed."* ]]; then
  printf '%s\n' 'symlinked temporary parent was not accepted' >&2
  exit 1
fi
unsafe_link="$test_root/unsafe-link"
ln -s "$unsafe_temp_parent" "$unsafe_link"
if "$smoke" "$fixture_root/smoke-pohunek" "$fixture_root/smoke-hermes" \
  --temp-parent-primary "$unsafe_link" \
  --temp-parent-fallback "$unsafe_link" >/dev/null 2>&1; then
  printf '%s\n' 'symlink into a repository unexpectedly passed' >&2
  exit 1
fi

# The plugin files must be non-empty: an empty file cannot come from embedded
# release assets.
if "$smoke" "$fixture_root/smoke-pohunek-empty-assets" "$fixture_root/smoke-hermes" \
  --temp-parent-primary "$safe_temp_parent" \
  --temp-parent-fallback "$safe_temp_parent" >/dev/null 2>&1; then
  printf '%s\n' 'empty plugin assets unexpectedly passed' >&2
  exit 1
fi

printf '%s\n' 'controlled Hermes release-plugin smoke passed'
