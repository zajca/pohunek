#!/bin/sh
# The stand-in `claude` the canary `capture` command drives in tests. It
# proves the driver, the record reader and the capture schema, not Claude.
#
# `--version` prints a banner inside the package range
# (`runtime-packages/claude/runtime.toml`). Otherwise it appends its arguments
# to `argv.log` beside `CLAUDE_CONFIG_DIR`, answers the OSC 7501 probe, draws
# the prompt box with a `state=idle` record and answers the canary prompts with
# the records the expected table names. Files in the working directory switch
# test-only modes:
#   untrusted     draw the folder-trust dialog, emit nothing
#   silent        answer no prompt with a record
#   login-record  emit `state=blocked:kind=auth` on the login screen
#   picker-record emit `state=idle` right before the theme picker, in the
#                 same printf
#   startup-idle  emit `state=idle` and keep the screen blank for a second
#                 before the theme picker
#   slow-start    show a blank screen for two seconds before the first screen
# An empty `CLAUDE_CONFIG_DIR` draws the first-run theme picker and, after a
# carriage return, the login method list; the bytes typed into the picker are
# appended to `theme-input.log` in the working directory. Input is read with
# `dd bs=1`, one read(2) per byte: BSD `head -c 1` reads the terminal through
# a stdio buffer and drops the rest of a prompt. Bytes typed into the
# folder-trust dialog are appended to `input.log` beside `argv.log`.
if [ "$1" = --version ]; then printf '%s\n' '2.1.322 (Claude Code)'; exit 0; fi
config=${CLAUDE_CONFIG_DIR:?}
logs=$(dirname "$config")
printf '%s\n' "$*" >> "$logs/argv.log"
stty raw -echo
if [ -e ./slow-start ]; then printf '\033[2J\033[H'; sleep 2; fi
emit() { printf '\033]7501;state=%s:app=claude-code\033\\' "$1"; }
hold() { while :; do sleep 1; done; }
rule='────────────────────────────────────────'
if [ -e ./untrusted ]; then
  printf '\033[2J\033[H%s\r\n Do you trust the files in this folder?\r\n ❯ 1. Yes, I trust this folder\r\n   2. No, exit\r\n Enter to confirm · Esc to cancel\r\n' "$rule"
  while :; do dd bs=1 count=1 2>/dev/null >> "$logs/input.log"; done
fi
cr=$(printf '\r')
if [ -z "$(ls -A "$config" 2>/dev/null)" ]; then
  if [ -e ./startup-idle ]; then emit idle; sleep 1; fi
  picker_record=
  if [ -e ./picker-record ]; then picker_record='\033]7501;state=idle:app=claude-code\033\\'; fi
  # One printf: the record comes right before the picker, without a pause.
  # shellcheck disable=SC2059
  printf "$picker_record\033[2J\033[H%s\r\n Choose the text style that looks best with your terminal\r\n ❯ Dark mode\r\n   Light mode\r\n" "$rule"
  while :; do
    c=$(dd bs=1 count=1 2>/dev/null)
    printf '%s' "$c" >> ./theme-input.log
    if [ "$c" = "$cr" ]; then break; fi
  done
  printf '\033[2J\033[H%s\r\n Select login method:\r\n ❯ 1. Claude account with subscription\r\n   2. Anthropic Console account\r\n' "$rule"
  if [ -e ./login-record ]; then emit 'blocked:kind=auth'; fi
  hold
fi
printf '\033]7501;?\033\\'
dd bs=1 count=10 2>/dev/null > /dev/null
printf '\033[2J\033[H%s\r\n❯ \r\n%s\r\n  ? for shortcuts' "$rule" "$rule"
emit idle
mcp_ready() {
  case " $* " in
    *" --mcp-config="*" --allowedTools=mcp__canary__ask "*) return 0 ;;
  esac
  return 1
}
args="$*"
submit() {
  if [ -e ./silent ]; then return; fi
  case "$1" in
    'Reply with the single word OK.') emit working; sleep 1; emit done ;;
    'Run the shell command touch canary-permission with the Bash tool.')
      emit working
      # A multi-byte character and a record each split across two writes,
      # so a reader poll can end inside either one.
      printf '\342\224'; sleep 1; printf '\200 Bash(true)\r\n'
      printf '\033]7501;state=blocked:kind=permi'; sleep 1
      printf 'ssion:app=claude-code:msg=U0VDUkVULVBST01QVC1URVhU\033\\' ;;
    'Call the ask tool of the canary MCP server.')
      # shellcheck disable=SC2086
      if mcp_ready $args; then emit 'blocked:kind=question'; else emit working; emit done; fi ;;
    'Write a 400 word story about a lighthouse.') emit working ;;
    # Any other text is a prompt the driver garbled or never sends.
    *) emit error ;;
  esac
}
pending=
while :; do
  c=$(dd bs=1 count=1 2>/dev/null)
  if [ "$c" = "$cr" ]; then submit "$pending"; pending=; else pending=$pending$c; fi
done
