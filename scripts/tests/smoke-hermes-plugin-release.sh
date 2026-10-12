#!/usr/bin/env bash
set -euo pipefail

script_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
fixture_root="$script_root/scripts/tests/fixtures"
smoke="$script_root/scripts/smoke-hermes-plugin-release"
test_root="$(mktemp -d /var/tmp/pohunek-smoke.XXXXXX)"
unsafe_temp_parent="$test_root/u"
safe_temp_parent="$test_root/s"
mkdir -p "$unsafe_temp_parent/.git" "$safe_temp_parent"

cleanup() {
  rm -rf -- "$test_root"
}
trap cleanup EXIT

# A controlled package, catalog and anchor. Only the package bytes matter: the
# controlled CLI reports the SHA-256 of the archive it was asked to install.
inputs="$test_root/inputs"
mkdir -p "$inputs"
printf '%s\n' 'controlled package archive' > "$inputs/hermes.tar.zst"
printf '%s\n' '{"controlled":"catalog"}' > "$inputs/runtime-catalog.json"
printf '%s\n' '{"controlled":"anchor"}' > "$inputs/runtime-catalog-anchor.json"

# Builds a release directory whose controlled CLI and daemon run in $2 mode.
release() {
  local directory="$test_root/release-$1-$2"
  mkdir -p "$directory"
  sed "s/^mode=ok\$/mode=$1/" "$fixture_root/smoke-pohunek" > "$directory/pohunek"
  sed "s/^mode=ok\$/mode=$2/" "$fixture_root/smoke-pohunekd" > "$directory/pohunekd"
  cp "$fixture_root/smoke-pohunek-sessiond" "$directory/pohunek-sessiond"
  chmod 755 "$directory/pohunek" "$directory/pohunekd" "$directory/pohunek-sessiond"
  printf '%s\n' "$directory"
}

run_smoke() {
  "$smoke" "$1" "$inputs/hermes.tar.zst" "$inputs/runtime-catalog.json" \
    "$inputs/runtime-catalog-anchor.json" "$fixture_root/smoke-hermes" \
    --temp-parent-primary "$2" \
    --temp-parent-fallback "$3"
}

# Runs the smoke expecting a failure whose stderr contains $1.
expect_failure() {
  local expected="$1"
  shift
  if run_smoke "$@" >/dev/null 2>"$test_root/failure-stderr"; then
    printf 'smoke unexpectedly passed, expected: %s\n' "$expected" >&2
    exit 1
  fi
  if ! grep -Fq -- "$expected" "$test_root/failure-stderr"; then
    printf 'smoke failed without the expected message %s:\n' "$expected" >&2
    cat "$test_root/failure-stderr" >&2
    exit 1
  fi
}

export POHUNEK_SMOKE_AMBIENT_SENTINEL="controlled-ambient-value"
export OPENAI_API_KEY="controlled-not-a-credential"
export HTTPS_PROXY="http://controlled.invalid"
export HERMES_API_KEY="controlled-not-a-credential"

ok_release="$(release ok ok)"

if run_smoke "$(release wrong-layout ok)" "$unsafe_temp_parent" "$safe_temp_parent" >/dev/null 2>&1; then
  printf '%s\n' 'wrong plugin layout unexpectedly passed' >&2
  exit 1
fi

output="$(run_smoke "$ok_release" "$unsafe_temp_parent" "$safe_temp_parent")"
if [[ "$output" != *"Hermes release-plugin smoke passed."* ]]; then
  printf '%s\n' 'controlled release smoke did not report success' >&2
  exit 1
fi
# The smoke stops the daemon it started and removes its private root.
if [[ -n "$(find "$safe_temp_parent" -mindepth 1 -maxdepth 1 -print)" ]]; then
  printf '%s\n' 'the smoke left its private root behind' >&2
  exit 1
fi

# An install from the CLI's embedded copy, or from another package, is not the
# package-backed install the release must prove.
expect_failure 'reported source=embedded' "$(release embedded ok)" "$safe_temp_parent" "$safe_temp_parent"
expect_failure 'did not report the installed Hermes package' "$(release wrong-digest ok)" "$safe_temp_parent" "$safe_temp_parent"

# A daemon that never accepts connections fails the smoke before any install.
expect_failure 'exited before it accepted connections' "$(release ok exits)" "$safe_temp_parent" "$safe_temp_parent"

# A daemon that ignores SIGTERM is killed after the exit bound and fails the
# smoke instead of hanging it. The bound is lowered to 5 polls (one second) so
# the case does not wait the production minute.
if "$smoke" "$(release ok ignores-term)" "$inputs/hermes.tar.zst" "$inputs/runtime-catalog.json" \
  "$inputs/runtime-catalog-anchor.json" "$fixture_root/smoke-hermes" \
  --temp-parent-primary "$safe_temp_parent" \
  --temp-parent-fallback "$safe_temp_parent" \
  --daemon-exit-attempts 5 >/dev/null 2>"$test_root/hung-stderr"; then
  printf '%s\n' 'a daemon ignoring SIGTERM unexpectedly passed' >&2
  exit 1
fi
grep -Fq 'did not exit within 5 polls of SIGTERM and was killed' "$test_root/hung-stderr"
hung_pid="$(sed -n 's/^controlled daemon pid \([0-9][0-9]*\)$/\1/p' "$test_root/hung-stderr")"
if [[ -z "$hung_pid" ]] || kill -0 "$hung_pid" 2>/dev/null; then
  printf '%s\n' 'the daemon that ignored SIGTERM is still running' >&2
  exit 1
fi

# A release directory without the daemon is refused before anything starts.
incomplete="$test_root/incomplete"
mkdir -p "$incomplete"
cp "$ok_release/pohunek" "$incomplete/pohunek"
if run_smoke "$incomplete" "$safe_temp_parent" "$safe_temp_parent" >/dev/null 2>&1; then
  printf '%s\n' 'a release without the daemon unexpectedly passed' >&2
  exit 1
fi

# macOS reaches its temporary directories through symlinks (/tmp, /var/tmp,
# $TMPDIR under /var). A symlinked parent that resolves outside any repository
# is accepted; one that resolves into a repository is refused.
symlink_parent="$test_root/symlinked-parent"
ln -s "$safe_temp_parent" "$symlink_parent"
output="$(run_smoke "$ok_release" "$symlink_parent" "$unsafe_temp_parent")"
if [[ "$output" != *"Hermes release-plugin smoke passed."* ]]; then
  printf '%s\n' 'symlinked temporary parent was not accepted' >&2
  exit 1
fi
unsafe_link="$test_root/unsafe-link"
ln -s "$unsafe_temp_parent" "$unsafe_link"
if run_smoke "$ok_release" "$unsafe_link" "$unsafe_link" >/dev/null 2>&1; then
  printf '%s\n' 'symlink into a repository unexpectedly passed' >&2
  exit 1
fi

# A parent too long for the daemon socket path is skipped, and the script fails
# with the byte budget when no parent fits. 60 characters exceed both the
# Darwin and the Linux budget.
long_parent="$safe_temp_parent/$(printf 'p%.0s' $(seq 1 60))"
mkdir -p "$long_parent"
output="$(run_smoke "$ok_release" "$long_parent" "$safe_temp_parent")"
if [[ "$output" != *"Hermes release-plugin smoke passed."* ]]; then
  printf '%s\n' 'too-long temporary parent did not fall through to a short one' >&2
  exit 1
fi
expect_failure 'short enough for the daemon socket path' "$ok_release" "$long_parent" "$long_parent"

# The plugin files must be non-empty: an empty file cannot come from the
# package's operator payload.
if run_smoke "$(release empty-assets ok)" "$safe_temp_parent" "$safe_temp_parent" >/dev/null 2>&1; then
  printf '%s\n' 'empty plugin assets unexpectedly passed' >&2
  exit 1
fi

printf '%s\n' 'controlled Hermes release-plugin smoke passed'
