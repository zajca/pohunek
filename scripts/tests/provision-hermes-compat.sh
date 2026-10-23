#!/usr/bin/env bash
set -euo pipefail

script_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
provision="$script_root/scripts/provision-hermes-compat"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/pohunek-hermes-provision-test.XXXXXX")"

cleanup() {
  rm -rf -- "$test_root"
}
trap cleanup EXIT

copy_root="$test_root/repository"
fake_bin="$test_root/bin"
mkdir -p "$copy_root/scripts" "$copy_root/compat/hermes" "$fake_bin"
cp "$provision" "$copy_root/scripts/"
cp "$script_root/compat/hermes/compatibility-lock.json" "$copy_root/compat/hermes/"

# The real python3 path is resolved here because the fakes' PATH is restricted.
real_python="$(command -v python3)"
git_marker="$test_root/git-started"
git_calls="$test_root/git-calls"
sleep_calls="$test_root/sleep-calls"
credential_marker="$test_root/credential-environment-leaked"
hang_marker="$test_root/git-fetch-hangs"
hung_pid_file="$test_root/hung-fetch-pid"
hung_child_pid_file="$test_root/hung-fetch-child-pid"
real_sleep="$(command -v sleep)"
cat >"$fake_bin/git" <<EOF
#!/usr/bin/env bash
touch "$git_marker"
printf '%s\n' "\$*" >>"$git_calls"
if [[ -n "\${PIP_INDEX_URL:-}" || -n "\${UV_INDEX_URL:-}" ]]; then
  touch "$credential_marker"
fi
# Local repository setup succeeds; only the network fetch fails, or hangs when
# the hang marker exists (a connect that never completes).
case " \$* " in
  *" fetch "*)
    if [[ -e "$hang_marker" ]]; then
      printf '%s\n' "\$\$" >"$hung_pid_file"
      "$real_sleep" 60 &
      printf '%s\n' "\$!" >"$hung_child_pid_file"
      wait
    fi
    exit 99
    ;;
esac
exit 0
EOF
cat >"$fake_bin/uv" <<'EOF'
#!/usr/bin/env bash
exit 99
EOF
cat >"$fake_bin/python3" <<EOF
#!/usr/bin/env bash
exec "$real_python" "\$@"
EOF
# The retry delay is recorded, not slept.
cat >"$fake_bin/sleep" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$sleep_calls"
EOF
chmod 0755 "$fake_bin/git" "$fake_bin/python3" "$fake_bin/uv" "$fake_bin/sleep"

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
# A failing fetch is retried a bounded number of times, each attempt with a
# stall bound, and then fails naming the tag and the attempt count.
test "$(grep -c " fetch " "$git_calls")" -eq 3
test "$(grep -c 'http.lowSpeedTime=60' "$git_calls")" -eq 3
test "$(grep -c 'http.lowSpeedLimit=1024' "$git_calls")" -eq 3
test "$(grep -cx 5 "$sleep_calls")" -eq 2
grep -Fq 'cannot fetch the locked tag v2026.8.3 from https://github.com/NousResearch/hermes-agent after 3 attempts' "$test_root/stderr"
grep -Fq 'fetching the locked tag v2026.8.3 (attempt 3 of 3)' "$test_root/stderr"

# A fetch attempt that never completes is cut off at its wall-clock cap, the
# stopped process is gone, and the failure names the cap. The cap is shortened
# in a copy so the test stays fast.
sed 's/^readonly FETCH_MAX_SECONDS=.*/readonly FETCH_MAX_SECONDS=1/' \
  "$provision" >"$copy_root/scripts/provision-hermes-compat-capped"
chmod 0755 "$copy_root/scripts/provision-hermes-compat-capped"
grep -Fq 'readonly FETCH_MAX_SECONDS=1' "$copy_root/scripts/provision-hermes-compat-capped"
touch "$hang_marker"
: >"$git_calls"
hang_started=$SECONDS
if PATH="$fake_bin:/usr/bin:/bin" \
  "$copy_root/scripts/provision-hermes-compat-capped" "$test_root/hung-fetch-install" \
  >"$test_root/stdout" 2>"$test_root/stderr"; then
  printf '%s\n' 'a hung fetch unexpectedly passed' >&2
  exit 1
fi
test "$((SECONDS - hang_started))" -lt 20
test "$(grep -c " fetch " "$git_calls")" -eq 3
test "$(grep -c 'the fetch attempt exceeded 1s and was stopped' "$test_root/stderr")" -eq 3
grep -Fq 'after 3 attempts (at most 1s each' "$test_root/stderr"
hung_pid="$(tail -n 1 "$hung_pid_file")"
if kill -0 "$hung_pid" 2>/dev/null; then
  printf '%s\n' 'a timed-out fetch process is still running' >&2
  exit 1
fi
hung_child_pid="$(tail -n 1 "$hung_child_pid_file")"
if kill -0 "$hung_child_pid" 2>/dev/null; then
  printf '%s\n' 'the transport helper of a timed-out fetch is still running' >&2
  kill "$hung_child_pid"
  exit 1
fi
rm -f "$hang_marker"

# Hosts with only BSD userland (macOS) have `shasum` but no `sha256sum` and no
# `realpath`. Run the provisioner with a PATH that offers exactly those tools:
# the shim below forwards `shasum -a 256` to the host digest tool, so the
# digest comparison itself still runs. The reviewed lock must reach controlled
# Git and a modified lock must still fail closed.
make_restricted_bin() {
  local dir="$1" tool
  mkdir -p "$dir"
  for tool in bash curl dirname env mkdir pkill tar touch uname; do
    ln -s "$(command -v "$tool")" "$dir/$tool"
  done
  ln -s "$fake_bin/git" "$dir/git"
  ln -s "$fake_bin/python3" "$dir/python3"
  ln -s "$fake_bin/uv" "$dir/uv"
  ln -s "$fake_bin/sleep" "$dir/sleep"
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

# A host without a pinned CPython runtime is refused before any upstream
# command runs.
unsupported_bin="$test_root/unsupported-bin"
make_restricted_bin "$unsupported_bin"
ln -s "$bsd_bin/shasum" "$unsupported_bin/shasum"
rm "$unsupported_bin/uname"
cat >"$unsupported_bin/uname" <<'UNAME'
#!/usr/bin/env bash
case "$1" in
  -s) printf '%s\n' FreeBSD ;;
  -m) printf '%s\n' amd64 ;;
  *) exit 97 ;;
esac
UNAME
chmod 0755 "$unsupported_bin/uname"
rm -f "$git_marker"
if PATH="$unsupported_bin" \
  "$copy_root/scripts/provision-hermes-compat" "$test_root/unsupported-install" \
  >"$test_root/stdout" 2>"$test_root/stderr"; then
  printf '%s\n' 'provisioner unexpectedly ran on a host without a pinned CPython' >&2
  exit 1
fi
grep -Fq 'no pinned CPython runtime for FreeBSD/amd64' "$test_root/stderr"
test ! -e "$git_marker"

# Bash 3.2 (macOS /bin/bash) has no readarray/mapfile and BSD userland has no
# GNU `realpath -e`; code lines (comments excluded) must not use them.
if grep -Ev '^[[:space:]]*#' "$provision" | grep -Eq 'readarray|mapfile|realpath -e'; then
  printf '%s\n' 'provisioner uses a bash 4 or GNU-only feature' >&2
  exit 1
fi

# The CPython download is bounded by connect, total and stall timeouts and a
# bounded retry, and a failed uv install names its phase (uv keeps its own
# defaults).
grep -Fq -- '--connect-timeout "$NETWORK_CONNECT_TIMEOUT_SECONDS"' "$provision"
grep -Fq -- '--max-time "$DOWNLOAD_MAX_SECONDS"' "$provision"
grep -Fq -- '--speed-limit "$NETWORK_STALL_BYTES" --speed-time "$NETWORK_STALL_SECONDS"' "$provision"
grep -Fq -- '--retry "$((NETWORK_ATTEMPTS - 1))"' "$provision"
grep -Fq 'cannot install the locked Hermes dependencies' "$provision"

# Keep the historical-lock workaround explicit and reject an accidental return
# to the re-resolving mode that failed during the M2 evidence capture.
grep -Fq 'uv sync --extra all --frozen' "$provision"
if grep -Eq 'uv sync .*--locked' "$provision"; then
  printf '%s\n' 'provisioner unexpectedly re-resolves the historical lock' >&2
  exit 1
fi
grep -Fq -- '-m venv --copies --without-pip' "$provision"
grep -Fq 'locked Python runtime escaped the isolated installation root' "$provision"
grep -Fq 'os.path.realpath(sys.argv[1])' "$provision"
# Only the interpreter's own shared libpython is copied: its soname on Linux,
# its install name on macOS. A LIBDIR glob would also copy unrelated builds
# such as another Python's library or the stable-ABI shim.
grep -Fq 'key = "LDLIBRARY" if sys.platform == "darwin" else "INSTSONAME"' "$provision"
grep -Fq 'sysconfig.get_config_var("Py_ENABLE_SHARED") != 1' "$provision"
grep -Fq 'cp -p "$python_library" "$venv_root/lib/${python_library##*/}"' "$provision"
if grep -Ev '^[[:space:]]*#' "$provision" | grep -Fq 'libpython*'; then
  printf '%s\n' 'provisioner copies libpython builds by glob' >&2
  exit 1
fi
# The venv runtime is the pinned, digest-verified standalone CPython; a macOS
# framework build (whose bin/python re-executes Python.app) is refused.
grep -Fq 'observed_python_archive_sha256' "$provision"
grep -Fq 'if sysconfig.get_config_var("PYTHONFRAMEWORK"):' "$provision"
grep -Fq '"$python_runtime" -I -c "$RUNTIME_CHECK_PROGRAM"' "$provision"
grep -Fq '"$python_runtime" -I -m venv --copies --without-pip "$venv_root"' "$provision"
grep -Fq 'pinned CPython runtime is unusable for the Hermes venv' "$provision"
if grep -Ev '^[[:space:]]*#' "$provision" | grep -Fq '"$python_bin" -m venv'; then
  printf '%s\n' 'provisioner builds the Hermes venv from the host Python' >&2
  exit 1
fi
# A uv /bin/sh trampoline in place of a direct shebang fails provisioning.
grep -Fq '"$python_bin" -c "$DIRECT_SHEBANG_PROGRAM" "$hermes_bin"' "$provision"
grep -Fq 'instead of a direct #!<python> script' "$provision"
grep -Fq 'choose a shorter install root without spaces' "$provision"
if grep -Fq 'uv venv' "$provision"; then
  printf '%s\n' 'provisioner unexpectedly creates an external-runtime symlink' >&2
  exit 1
fi

printf '%s\n' 'controlled Hermes provisioning checks passed'
