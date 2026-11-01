#!/usr/bin/env bash
# Cancels the current run and names the job that failed first. The display
# name and the failed step come from the jobs API on a best-effort basis: the
# cancel does not depend on them, and the job id (`github.job`, shared by every
# matrix leg) stays in the message as the fallback. The lookup runs only with a
# non-empty JOB_ID and is cut off after LOOKUP_TIMEOUT_SECONDS by a watchdog
# (GNU `timeout` is not available on the macOS callers); a timeout falls back
# like an API error. An unset LOOKUP_TIMEOUT_SECONDS skips the lookup.
set -u

: "${GH_TOKEN:?}" "${RUN_ID:?}" "${REPOSITORY:?}" "${FAILED_JOB:?}"
JOB_ID="${JOB_ID:-}"
LOOKUP_TIMEOUT_SECONDS="${LOOKUP_TIMEOUT_SECONDS:-}"

# Prints the jobs API reply; fails on an API error or when the call outlives the timeout.
bounded_job_lookup() {
  local reply gh_pid watchdog_pid rc
  reply="$(mktemp)" || return 1
  gh api "repos/${REPOSITORY}/actions/jobs/${JOB_ID}" >"$reply" 2>/dev/null &
  gh_pid=$!
  (sleep "$LOOKUP_TIMEOUT_SECONDS" && kill "$gh_pid") >/dev/null 2>&1 &
  watchdog_pid=$!
  wait "$gh_pid" 2>/dev/null
  rc=$?
  kill "$watchdog_pid" >/dev/null 2>&1
  wait "$watchdog_pid" >/dev/null 2>&1
  if [ "$rc" -eq 0 ]; then
    cat "$reply"
  fi
  rm -f "$reply"
  return "$rc"
}

name=""
step=""
if [ -n "$JOB_ID" ] && [ -n "$LOOKUP_TIMEOUT_SECONDS" ] && info="$(bounded_job_lookup)"; then
  name="$(jq -r '.name // empty' <<<"$info" 2>/dev/null | tr -d '\r\n' || true)"
  step="$(jq -r '[.steps[]? | select(.conclusion == "failure")][0].name // empty' <<<"$info" 2>/dev/null | tr -d '\r\n' || true)"
fi

label="${name:-$FAILED_JOB}"
if [ -n "$JOB_ID" ]; then
  where="${FAILED_JOB}, https://github.com/${REPOSITORY}/actions/runs/${RUN_ID}/job/${JOB_ID}"
else
  where="${FAILED_JOB}, https://github.com/${REPOSITORY}/actions/runs/${RUN_ID}"
fi
message="job '${label}' (${where}) failed first"
if [ -n "$step" ]; then
  message="${message} in step '${step}'"
fi
message="${message}; cancelling run ${RUN_ID}, so the jobs it cancels are not failures of their own"

echo "::error title=Fail fast::${message}"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  printf '### Fail fast\n\n%s\n' "$message" >> "$GITHUB_STEP_SUMMARY"
fi
gh run cancel "$RUN_ID" --repo "$REPOSITORY"
