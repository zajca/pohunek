#!/bin/sh
# Behavioral scenario for runtime-packages/claude/bin/claude-accounts and the
# account wrapper it generates. Everything runs against a fresh work root with
# a fake `claude` and a fake `pohunek` on PATH, a private HOME and synthetic
# token values; the owner's real configuration and daemon are never touched.
#
# usage: claude-accounts-scenario.sh <package-dir> <work-root>
#
# <work-root> must exist and be empty. The `add` runs go through `script` so the
# setup program sees a real terminal, the same as `pohunek plugin run`.

set -eu

pkg=$1
root=$2
setup=$pkg/bin/claude-accounts
home=$root/home
acc=$home/.config/pohunek/claude-accounts
fake=$root/fakebin
wrapper=$acc/claude-account
path_value=$fake:/usr/bin:/bin
exec_log=$root/exec.log
pohunek_log=$root/pohunek.log
options_copy=$root/options-copy.toml
next_token=$root/next-token
transcript=$root/transcript.typescript

# Synthetic markers only. Their shape is the accepted length and character set.
TOKEN_A=sk-ant-oat01-synthetic-a-0123456789abcdef0123456789abcdef0123456789
TOKEN_B=sk-ant-oat01-synthetic-b-fedcba9876543210fedcba9876543210fedcba9876
TOKEN_C=sk-ant-oat01-synthetic-c-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

failures=0

ok() {
    printf 'ok   %s\n' "$1"
}

# check NAME COMMAND... : runs COMMAND, records the outcome under NAME.
check() {
    name=$1
    shift
    if "$@"; then
        ok "$name"
    else
        printf 'FAIL %s\n' "$name" >&2
        failures=$((failures + 1))
    fi
}

mode_is() {
    [ -n "$(find "$1" -prune -perm "$2" -print)" ]
}

contains() {
    [ -f "$1" ] && grep -q -F -- "$2" "$1"
}

absent() {
    [ ! -e "$1" ] && [ ! -L "$1" ]
}

# Succeeds when FILE does not exist or does not contain VALUE.
absent_in_file() {
    [ ! -f "$1" ] || ! grep -q -F -- "$2" "$1"
}

# Succeeds when no argv line of the fake claude's exec log holds VALUE.
argv_lacks() {
    [ ! -f "$1" ] || ! grep '^argv:' "$1" | grep -q -F -- "$2"
}

# Counts the lines of FILE that hold the synthetic value VALUE (grep -o gives
# one line per occurrence).
occurrences() {
    if [ -f "$1" ]; then
        grep -o -F -- "$2" "$1" | awk 'END { print NR }'
    else
        printf '0\n'
    fi
}

is_count() {
    [ "$(occurrences "$1" "$2")" -eq "$3" ]
}

# Quotes one word for a shell command string handed to `script -c`.
shq() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# run_plain ARGS... : runs the setup program with stdin from /dev/null and a
# clean environment, so the TTY refusal applies.
run_plain() {
    env -i HOME="$home" PATH="$path_value" LANG=C "$@" </dev/null >/dev/null 2>"$root/stderr.txt"
}

# run_pty INPUT ARGS... : runs ARGS under a pseudo-terminal. INPUT, when not
# empty, is typed after a delay, so `claude setup-token` has finished and the
# hidden prompt is active when the value arrives.
run_pty() {
    input=$1
    shift
    line=''
    for word in env -i "HOME=$home" "PATH=$path_value" LANG=C "$@"; do
        line="$line $(shq "$word")"
    done
    if [ -n "$input" ]; then
        rc=0
        (sleep 1; printf '%s\n' "$input") | script -q -e -c "$line" "$transcript" >/dev/null 2>"$root/stderr.txt" || rc=$?
    else
        rc=0
        script -q -e -c "$line" "$transcript" </dev/null >/dev/null 2>"$root/stderr.txt" || rc=$?
    fi
    return "$rc"
}

reset_transcript() {
    : > "$transcript"
}

# ---------------------------------------------------------------- fixtures

mkdir -p "$home" "$fake"
printf '%s\n' "$TOKEN_A" > "$next_token"

# Fake claude: answers --version, prints the token on setup-token (as the real
# command prints it on the terminal) and records what an exec'd session sees,
# names and values, never through argv.
cat > "$fake/claude" <<'EOF'
#!/bin/sh
case ${1:-} in
    --version)
        printf '2.1.320 (Claude Code)\n'
        exit 0
        ;;
    setup-token)
        printf 'Token: %s\n' "$(cat "$(dirname "$0")/../next-token")"
        exit 0
        ;;
esac
log=$(dirname "$0")/../exec.log
{
    printf 'argv:'
    for a in "$@"; do printf ' [%s]' "$a"; done
    printf '\n'
    for name in ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_USE_BEDROCK CLAUDE_CONFIG_DIR CLAUDE_CODE_SIMPLE; do
        if [ -n "$(eval "printf '%s' \"\${$name-}\"")" ]; then
            printf 'present %s\n' "$name"
        fi
    done
    if [ -n "${CLAUDE_CODE_OAUTH_TOKEN-}" ]; then
        printf 'present CLAUDE_CODE_OAUTH_TOKEN=%s\n' "$CLAUDE_CODE_OAUTH_TOKEN"
    fi
    printf 'end\n'
} >> "$log"
exit 0
EOF
chmod 755 "$fake/claude"

# Fake pohunek: records argv, copies the --options-file contents, and answers
# `plugin profile list --json --all` from listing.json when the test wrote one.
cat > "$fake/pohunek" <<'EOF'
#!/bin/sh
root=$(dirname "$0")/..
{
    printf 'argv:'
    for a in "$@"; do printf ' [%s]' "$a"; done
    printf '\n'
} >> "$root/pohunek.log"
prev=''
for a in "$@"; do
    if [ "$prev" = --options-file ]; then
        cat "$a" > "$root/options-copy.toml"
    fi
    prev=$a
done
if [ "$*" = "plugin profile list --json --all" ]; then
    if [ -f "$root/listing.json" ]; then
        cat "$root/listing.json"
    else
        printf '{"profiles":[],"hidden":0,"truncated":false}\n'
    fi
fi
exit 0
EOF
chmod 755 "$fake/pohunek"

# ----------------------------------------------------------- refusal paths

run_plain "$setup" add work --login || true
check "no TTY on stdin refuses and creates nothing" absent "$acc"

reset_transcript
run_pty "" POHUNEK_SESSION_ID=s-1 "$setup" add work --login || true
check "inside POHUNEK_SESSION_ID refuses and creates nothing" absent "$acc"

reset_transcript
run_pty "" "$setup" add Bad_Name || true
check "uppercase or underscore name refused" absent "$acc"
run_pty "" "$setup" add acct --login || true
check "name acct refused (it equals the parent profile)" absent "$acc"

# -------------------------------------------------------------- login account

reset_transcript
run_pty "" "$setup" add work --login
check "login account is added" test -f "$acc/work.login"
check "accounts directory is mode 0700" mode_is "$acc" 700
check "login marker is mode 0600" mode_is "$acc/work.login" 600
check "login account writes no token file" absent "$acc/work.token"
check "wrapper is mode 0755" mode_is "$wrapper" 755
check "wrapper names the accounts directory and the resolved claude" contains "$wrapper" "CLAUDE_BIN='$fake/claude'"
check "pohunek receives the parent profile with --program and --options-file" \
    contains "$pohunek_log" "[--program] [$wrapper]"
check "pohunek receives the locked preset of the login account" \
    contains "$pohunek_log" "[--opt] [account=work] [--locked]"
check "the options file declares the account choice" contains "$options_copy" 'choices.work = { label = "work", env = { CLAUDE_ACCOUNT = "work" } }'
check "the login account is the default choice" contains "$options_copy" 'default = "work"'

# A second stored login is one account too many.
reset_transcript
run_pty "" "$setup" add other --login || true
check "a second stored login is refused" absent "$acc/other.login"

# ---------------------------------------------------------------- token account

reset_transcript
run_pty "$TOKEN_A" "$setup" add personal
check "token account is added" test -f "$acc/personal.token"
check "token file is mode 0600" mode_is "$acc/personal.token" 600
check "token file holds the pasted value" contains "$acc/personal.token" "$TOKEN_A"
check "the pasted value is not echoed: only setup-token printed it" is_count "$transcript" "$TOKEN_A" 1
check "the transcript carries the clear-screen sequence" contains "$transcript" "[2J"
check "the token is absent from the pohunek argv log" absent_in_file "$pohunek_log" "$TOKEN_A"
check "the token is absent from the wrapper" absent_in_file "$wrapper" "$TOKEN_A"
check "the preset of the token account is written" contains "$pohunek_log" "[--opt] [account=personal] [--locked]"

# Duplicate value under another name.
reset_transcript
printf '%s\n' "$TOKEN_A" > "$next_token"
run_pty "$TOKEN_A" "$setup" add side || true
check "duplicate token under a second name is refused" absent "$acc/side.token"

# Existing name without --replace.
reset_transcript
printf '%s\n' "$TOKEN_B" > "$next_token"
run_pty "$TOKEN_B" "$setup" add personal || true
check "existing name without --replace is refused" contains "$acc/personal.token" "$TOKEN_A"

# Short value.
reset_transcript
printf 'short\n' > "$next_token"
run_pty "short" "$setup" add tiny || true
check "a value outside the length bounds is refused" absent "$acc/tiny.token"

# --replace changes the value.
reset_transcript
run_pty "$TOKEN_B" "$setup" add personal --replace
check "--replace stores the new value" contains "$acc/personal.token" "$TOKEN_B"
check "--replace keeps the file at mode 0600" mode_is "$acc/personal.token" 600

# A listing that reports the parent profile digest makes the next create replace it.
digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
printf '{"profiles":[{"name":"claude-acct","file_digest":"%s"}],"hidden":0,"truncated":false}\n' "$digest" > "$root/listing.json"
reset_transcript
printf '%s\n' "$TOKEN_C" > "$next_token"
run_pty "$TOKEN_C" "$setup" add side
check "the parent profile is replaced with the listed digest" contains "$pohunek_log" "[--replace] [$digest]"
check "a new account stores its value" contains "$acc/side.token" "$TOKEN_C"
rm -f "$root/listing.json"

# ----------------------------------------------------------------- wrapper

# Token account: the inherited credentials are removed, the token comes from the
# file, and CLAUDE_CONFIG_DIR is left as found.
: > "$exec_log"
env -i HOME="$home" PATH="$path_value" LANG=C CLAUDE_ACCOUNT=personal \
    ANTHROPIC_API_KEY=synthetic-key ANTHROPIC_AUTH_TOKEN=synthetic-auth \
    CLAUDE_CODE_USE_BEDROCK=1 CLAUDE_CONFIG_DIR=/keep CLAUDE_CODE_OAUTH_TOKEN=stale-value \
    "$wrapper" chat
check "token account exec'd claude with the stored token" contains "$exec_log" "present CLAUDE_CODE_OAUTH_TOKEN=$TOKEN_B"
check "token account drops the stale inherited token" absent_in_file "$exec_log" stale-value
check "token account clears ANTHROPIC_API_KEY" absent_in_file "$exec_log" "present ANTHROPIC_API_KEY"
check "token account clears ANTHROPIC_AUTH_TOKEN" absent_in_file "$exec_log" "present ANTHROPIC_AUTH_TOKEN"
check "token account clears the Bedrock selector" absent_in_file "$exec_log" "present CLAUDE_CODE_USE_BEDROCK"
check "CLAUDE_CONFIG_DIR is left as found" contains "$exec_log" "present CLAUDE_CONFIG_DIR"
check "the token is not in claude's argv" argv_lacks "$exec_log" "$TOKEN_B"

# Login account: no token reaches claude even when one is inherited.
: > "$exec_log"
env -i HOME="$home" PATH="$path_value" LANG=C CLAUDE_ACCOUNT=work \
    CLAUDE_CODE_OAUTH_TOKEN=stale-value ANTHROPIC_API_KEY=synthetic-key \
    "$wrapper" chat
check "login account exec's claude without a token" absent_in_file "$exec_log" "CLAUDE_CODE_OAUTH_TOKEN"
check "login account clears ANTHROPIC_API_KEY" absent_in_file "$exec_log" "present ANTHROPIC_API_KEY"

# The daemon's version probe needs no account at all.
: > "$exec_log"
env -i HOME="$home" PATH="$path_value" LANG=C "$wrapper" --version >"$root/probe.txt"
check "version probe runs without a selector" contains "$root/probe.txt" "2.1.320 (Claude Code)"

# Refusals run nothing.
before=$(awk 'END { print NR }' "$exec_log")
refuse() {
    if env -i HOME="$home" PATH="$path_value" LANG=C "$@" >/dev/null 2>&1; then
        return 1
    fi
    after=$(awk 'END { print NR }' "$exec_log")
    [ "$after" -eq "$before" ]
}
check "bad selector is refused" refuse CLAUDE_ACCOUNT=Bad_Name "$wrapper" chat
check "unknown account is refused" refuse CLAUDE_ACCOUNT=ghost "$wrapper" chat
check "--bare is refused" refuse CLAUDE_ACCOUNT=personal "$wrapper" --bare
check "CLAUDE_CODE_SIMPLE is refused" refuse CLAUDE_ACCOUNT=personal CLAUDE_CODE_SIMPLE=1 "$wrapper" chat

chmod 644 "$acc/personal.token"
check "wrong token mode is refused" refuse CLAUDE_ACCOUNT=personal "$wrapper" chat
chmod 600 "$acc/personal.token"

ln -s "$acc/personal.token" "$acc/linked.token"
check "symbolic-link token is refused" refuse CLAUDE_ACCOUNT=linked "$wrapper" chat
rm -f "$acc/linked.token"

: > "$acc/empty.token"
chmod 600 "$acc/empty.token"
check "empty token file is refused" refuse CLAUDE_ACCOUNT=empty "$wrapper" chat
rm -f "$acc/empty.token"

# ------------------------------------------------------------ list and logs

listing=$(env -i HOME="$home" PATH="$path_value" LANG=C "$setup" list)
case $listing in
    *'personal token ok'*) ok "list shows the token account with its mode" ;;
    *) printf 'FAIL list shows the token account\n' >&2; failures=$((failures + 1)) ;;
esac
case $listing in
    *'work login ok'*) ok "list shows the login account" ;;
    *) printf 'FAIL list shows the login account\n' >&2; failures=$((failures + 1)) ;;
esac

clean=$(env -i HOME="$home" PATH="$path_value" LANG=C "$setup" check-logs)
case $clean in
    '0 files'*) ok "check-logs reports no file when no token is logged" ;;
    *) printf 'FAIL check-logs reports no file\n' >&2; failures=$((failures + 1)) ;;
esac

mkdir -p "$home/.local/state/pohunek/logs"
printf '{"msg":"planted %s"}\n' "$TOKEN_B" > "$home/.local/state/pohunek/logs/pohunekd.jsonl"
found=$(env -i HOME="$home" PATH="$path_value" LANG=C "$setup" check-logs)
case $found in
    '1 files'*pohunekd.jsonl*) ok "check-logs names the planted file" ;;
    *) printf 'FAIL check-logs names the planted file\n' >&2; failures=$((failures + 1)) ;;
esac
case $found in
    *"$TOKEN_B"*) printf 'FAIL check-logs printed token content\n' >&2; failures=$((failures + 1)) ;;
    *) ok "check-logs prints no token content" ;;
esac

# Last, because chmod does not clear the setgid bit on every filesystem, so the
# directory is left at 2700 for the rest of the run; the work root is discarded.
chmod 2700 "$acc"
check "setgid accounts directory is refused" refuse CLAUDE_ACCOUNT=personal "$wrapper" chat

if [ "$failures" -ne 0 ]; then
    printf '%s check(s) failed\n' "$failures" >&2
    exit 1
fi
printf 'all checks passed\n'
