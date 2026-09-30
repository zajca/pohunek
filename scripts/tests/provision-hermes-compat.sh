#!/usr/bin/env bash
set -euo pipefail

script_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
provision="$script_root/scripts/provision-hermes-compat"
test_root="$(mktemp -d /tmp/pohunek-hermes-provision-test.XXXXXX)"

cleanup() {
  rm -rf -- "$test_root"
}
trap cleanup EXIT

copy_root="$test_root/repository"
fake_bin="$test_root/bin"
mkdir -p "$copy_root/scripts" "$copy_root/compat/hermes" "$fake_bin"
cp "$provision" "$copy_root/scripts/"
cp "$script_root/compat/hermes/compatibility-lock.json" "$copy_root/compat/hermes/"

# Absolute paths for the fakes are resolved here: their PATH is restricted and
# macOS keeps `cat` in /bin while `python3` lives in /usr/bin.
cat_bin="$(command -v cat)"
real_python="$(command -v python3)"
git_marker="$test_root/git-started"
credential_marker="$test_root/credential-environment-leaked"
cat >"$fake_bin/git" <<EOF
#!/usr/bin/env bash
touch "$git_marker"
if [[ -n "\${PIP_INDEX_URL:-}" || -n "\${UV_INDEX_URL:-}" ]]; then
  touch "$credential_marker"
fi
exit 99
EOF
cat >"$fake_bin/uv" <<'EOF'
#!/usr/bin/env bash
exit 99
EOF
cat >"$fake_bin/python3" <<EOF
#!/usr/bin/env bash
if [[ "\$#" -eq 3 && "\$1" == "-" ]]; then
  "$cat_bin" >/dev/null
  exit 0
fi
exec "$real_python" "\$@"
EOF
chmod 0755 "$fake_bin/git" "$fake_bin/python3" "$fake_bin/uv"

# A modified lock must fail before any upstream command can execute.
printf '\n' >>"$copy_root/compat/hermes/compatibility-lock.json"
if PATH="$fake_bin:/usr/bin:/bin" \
  "$copy_root/scripts/provision-hermes-compat" "$test_root/modified-lock-install" \
  >"$test_root/stdout" 2>"$test_root/stderr"; then
  printf '%s\n' 'modified compatibility lock unexpectedly passed' >&2
  exit 1
fi
grep -Fq 'compatibility lock SHA-256 does not match the reviewed digest' "$test_root/stderr"
test ! -e "$git_marker"

# The reviewed lock reaches provenance acquisition, where the controlled fake
# stops the test before any network access or upstream code execution.
cp "$script_root/compat/hermes/compatibility-lock.json" "$copy_root/compat/hermes/"
if PIP_INDEX_URL="https://example.invalid/private" \
  UV_INDEX_URL="https://example.invalid/private" \
  PATH="$fake_bin:/usr/bin:/bin" \
  "$copy_root/scripts/provision-hermes-compat" "$test_root/reviewed-lock-install" \
  >"$test_root/stdout" 2>"$test_root/stderr"; then
  printf '%s\n' 'controlled Git failure unexpectedly passed' >&2
  exit 1
fi
if [[ ! -f "$git_marker" ]]; then
  printf '%s\n' 'reviewed compatibility lock did not reach controlled Git' >&2
  sed -n '1,20p' "$test_root/stderr" >&2
  exit 1
fi
if [[ -e "$credential_marker" ]]; then
  printf '%s\n' 'credential-bearing package index environment reached Git' >&2
  exit 1
fi

# Hosts with only BSD userland (macOS) have `shasum` but no `sha256sum` and no
# `realpath`. Run the provisioner with a PATH that offers exactly those tools:
# the shim below forwards `shasum -a 256` to the host digest tool, so the
# digest comparison itself still runs. The reviewed lock must reach controlled
# Git and a modified lock must still fail closed.
make_restricted_bin() {
  local dir="$1" tool
  mkdir -p "$dir"
  for tool in bash dirname env mkdir touch; do
    ln -s "$(command -v "$tool")" "$dir/$tool"
  done
  ln -s "$fake_bin/git" "$dir/git"
  ln -s "$fake_bin/python3" "$dir/python3"
  ln -s "$fake_bin/uv" "$dir/uv"
}

bsd_bin="$test_root/bsd-bin"
make_restricted_bin "$bsd_bin"
if command -v sha256sum >/dev/null 2>&1; then
  digest_command="$(command -v sha256sum)"
else
  digest_command="$(command -v shasum) -a 256"
fi
cat >"$bsd_bin/shasum" <<SHIM
#!/usr/bin/env bash
if [[ "\$#" -lt 2 || "\$1" != "-a" || "\$2" != "256" ]]; then
  exit 97
fi
shift 2
exec $digest_command "\$@"
SHIM
chmod 0755 "$bsd_bin/shasum"
for absent in sha256sum realpath; do
  if PATH="$bsd_bin" command -v "$absent" >/dev/null 2>&1; then
    printf '%s\n' "BSD-only PATH unexpectedly offers $absent" >&2
    exit 1
  fi
done

rm -f "$git_marker"
if PATH="$bsd_bin" \
  "$copy_root/scripts/provision-hermes-compat" "$test_root/bsd-reviewed-lock-install" \
  >"$test_root/stdout" 2>"$test_root/stderr"; then
  printf '%s\n' 'controlled Git failure unexpectedly passed with shasum only' >&2
  exit 1
fi
if [[ ! -f "$git_marker" ]]; then
  printf '%s\n' 'shasum-only run did not reach controlled Git' >&2
  sed -n '1,20p' "$test_root/stderr" >&2
  exit 1
fi

printf '\n' >>"$copy_root/compat/hermes/compatibility-lock.json"
rm -f "$git_marker"
if PATH="$bsd_bin" \
  "$copy_root/scripts/provision-hermes-compat" "$test_root/bsd-modified-lock-install" \
  >"$test_root/stdout" 2>"$test_root/stderr"; then
  printf '%s\n' 'modified compatibility lock unexpectedly passed with shasum only' >&2
  exit 1
fi
grep -Fq 'compatibility lock SHA-256 does not match the reviewed digest' "$test_root/stderr"
test ! -e "$git_marker"
cp "$script_root/compat/hermes/compatibility-lock.json" "$copy_root/compat/hermes/"

# Without either digest tool the provisioner refuses before any upstream command.
no_digest_bin="$test_root/no-digest-bin"
make_restricted_bin "$no_digest_bin"
if PATH="$no_digest_bin" \
  "$copy_root/scripts/provision-hermes-compat" "$test_root/no-digest-install" \
  >"$test_root/stdout" 2>"$test_root/stderr"; then
  printf '%s\n' 'provisioner unexpectedly ran without a SHA-256 tool' >&2
  exit 1
fi
grep -Fq 'sha256sum or shasum' "$test_root/stderr"
test ! -e "$git_marker"

# Bash 3.2 (macOS /bin/bash) has no readarray/mapfile and BSD userland has no
# GNU `realpath -e`; code lines (comments excluded) must not use them.
if grep -Ev '^[[:space:]]*#' "$provision" | grep -Eq 'readarray|mapfile|realpath -e'; then
  printf '%s\n' 'provisioner uses a bash 4 or GNU-only feature' >&2
  exit 1
fi

# Keep the historical-lock workaround explicit and reject an accidental return
# to the re-resolving mode that failed during the M2 evidence capture.
grep -Fq 'uv sync --extra all --frozen' "$provision"
if grep -Eq 'uv sync .*--locked' "$provision"; then
  printf '%s\n' 'provisioner unexpectedly re-resolves the historical lock' >&2
  exit 1
fi
grep -Fq -- '-m venv --copies --without-pip' "$provision"
grep -Fq 'locked Python runtime escaped the isolated installation root' "$provision"
if grep -Fq 'uv venv' "$provision"; then
  printf '%s\n' 'provisioner unexpectedly creates an external-runtime symlink' >&2
  exit 1
fi

printf '%s\n' 'controlled Hermes provisioning checks passed'
