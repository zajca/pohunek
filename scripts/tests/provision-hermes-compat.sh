#!/usr/bin/env bash
set -euo pipefail

script_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
provision="$script_root/scripts/provision-hermes-compat"
test_root="$(/usr/bin/mktemp -d /tmp/pohunek-hermes-provision-test.XXXXXX)"

cleanup() {
  /usr/bin/rm -rf -- "$test_root"
}
trap cleanup EXIT

copy_root="$test_root/repository"
fake_bin="$test_root/bin"
mkdir -p "$copy_root/scripts" "$copy_root/compat/hermes" "$fake_bin"
cp "$provision" "$copy_root/scripts/"
cp "$script_root/compat/hermes/compatibility-lock.json" "$copy_root/compat/hermes/"

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
cat >"$fake_bin/python3" <<'EOF'
#!/usr/bin/env bash
if [[ "$#" -eq 3 && "$1" == "-" ]]; then
  /usr/bin/cat >/dev/null
  exit 0
fi
exec /usr/bin/python3 "$@"
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
