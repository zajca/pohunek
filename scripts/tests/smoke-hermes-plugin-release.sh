#!/usr/bin/env bash
set -euo pipefail

script_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
fixture_root="$script_root/scripts/tests/fixtures"
smoke="$script_root/scripts/smoke-hermes-plugin-release"
test_root="$(/usr/bin/mktemp -d /var/tmp/pohunek-hermes-smoke-test.XXXXXX)"
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
printf '%s\n' 'controlled Hermes release-plugin smoke passed'
