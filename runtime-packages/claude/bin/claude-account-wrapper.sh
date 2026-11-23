#!/bin/sh
# Launch wrapper of one Claude subscription. `claude-accounts add` writes a copy
# into the 0700 accounts directory, prepends the two assignments that name the
# accounts directory and the real claude binary, and points the claude-acct
# profile at the copy. This template is the rest of that file; never edit a
# generated copy by hand.
#
# The account is chosen by CLAUDE_ACCOUNT, which the profile choice sets. The
# token stays in the account file: it is read into the environment of the
# exec'd claude only, never placed in argv or written anywhere.

# BEGIN-TEMPLATE
set -eu
umask 077
LC_ALL=C
export LC_ALL

fail() {
    printf 'claude-account: %s\n' "$*" >&2
    exit 1
}

# The daemon probes the program with `--version` alone and without any
# credential, so that invocation is answered before any account check.
if [ "$#" -eq 1 ] && [ "$1" = "--version" ]; then
    exec "$CLAUDE_BIN" --version
fi

selector=${CLAUDE_ACCOUNT:-}
[ -n "$selector" ] || fail "CLAUDE_ACCOUNT is not set"
case $selector in
    *[!a-z0-9-]*) fail "invalid account selector" ;;
esac
[ "${#selector}" -le 32 ] || fail "invalid account selector"

# Bare mode ignores CLAUDE_CODE_OAUTH_TOKEN, so the token would be silently
# unused; CLAUDE_CODE_SIMPLE switches bare mode on through the environment.
for arg in "$@"; do
    case $arg in
        --bare) fail "--bare is not supported with an account: bare mode does not read the account token" ;;
    esac
done
[ -z "${CLAUDE_CODE_SIMPLE:-}" ] || fail "CLAUDE_CODE_SIMPLE is set: bare mode does not read the account token"

uid=$(id -u)
if [ -L "$ACCOUNTS_DIR" ] || [ ! -d "$ACCOUNTS_DIR" ]; then
    fail "accounts directory is missing or a symbolic link"
fi
if [ -z "$(find "$ACCOUNTS_DIR" -prune -type d -perm 700 -user "$uid" -print)" ]; then
    fail "accounts directory must be mode 0700 and owned by you"
fi

token_file=$ACCOUNTS_DIR/$selector.token
login_file=$ACCOUNTS_DIR/$selector.login
if [ -e "$token_file" ] || [ -L "$token_file" ]; then
    kind=token
elif [ -e "$login_file" ] || [ -L "$login_file" ]; then
    kind=login
else
    fail "no account named by the selector"
fi

# Every credential that outranks the account's OAuth token, plus the profile
# sources that outrank the stored login, is removed from the inherited
# environment before the choice is applied. CLAUDE_CONFIG_DIR is left as found
# because the subscriptions share one home.
unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_API_KEY_HELPER_TTL_MS
unset CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_VERTEX CLAUDE_CODE_USE_FOUNDRY
unset ANTHROPIC_PROFILE ANTHROPIC_FEDERATION_RULE_ID ANTHROPIC_ORGANIZATION_ID
unset CLAUDE_CODE_OAUTH_TOKEN

if [ "$kind" = token ]; then
    if [ -L "$token_file" ] || [ ! -f "$token_file" ]; then
        fail "token file is not a regular file"
    fi
    if [ -z "$(find "$token_file" -prune -type f -perm 600 -user "$uid" -print)" ]; then
        fail "token file must be mode 0600 and owned by you"
    fi
    [ -s "$token_file" ] || fail "token file is empty"
    IFS= read -r oauth_token < "$token_file" || :
    [ -n "${oauth_token:-}" ] || fail "token file holds no token"
    CLAUDE_CODE_OAUTH_TOKEN=$oauth_token
    export CLAUDE_CODE_OAUTH_TOKEN
    unset oauth_token
else
    if [ -L "$login_file" ] || [ ! -f "$login_file" ]; then
        fail "login marker is not a regular file"
    fi
    if [ -z "$(find "$login_file" -prune -type f -perm 600 -user "$uid" -print)" ]; then
        fail "login marker must be mode 0600 and owned by you"
    fi
fi

[ -x "$CLAUDE_BIN" ] || fail "the claude binary recorded at generation is not executable"
exec "$CLAUDE_BIN" "$@"
