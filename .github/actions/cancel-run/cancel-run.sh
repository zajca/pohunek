#!/usr/bin/env bash
# Cancels the current run and names the job that failed first. The display
# name and the failed step come from the jobs API on a best-effort basis: the
# cancel does not depend on them, and the job id (`github.job`, shared by every
# matrix leg) stays in the message as the fallback.
set -u

: "${GH_TOKEN:?}" "${RUN_ID:?}" "${REPOSITORY:?}" "${FAILED_JOB:?}" "${JOB_ID:?}"

name=""
step=""
if info="$(gh api "repos/${REPOSITORY}/actions/jobs/${JOB_ID}" 2>/dev/null)"; then
  name="$(jq -r '.name // empty' <<<"$info" 2>/dev/null | tr -d '\r\n' || true)"
  step="$(jq -r '[.steps[]? | select(.conclusion == "failure")][0].name // empty' <<<"$info" 2>/dev/null | tr -d '\r\n' || true)"
fi

label="${name:-$FAILED_JOB}"
message="job '${label}' (${FAILED_JOB}, https://github.com/${REPOSITORY}/actions/runs/${RUN_ID}/job/${JOB_ID}) failed first"
if [ -n "$step" ]; then
  message="${message} in step '${step}'"
fi
message="${message}; cancelling run ${RUN_ID}, so the jobs it cancels are not failures of their own"

echo "::error title=Fail fast::${message}"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  printf '### Fail fast\n\n%s\n' "$message" >> "$GITHUB_STEP_SUMMARY"
fi
gh run cancel "$RUN_ID" --repo "$REPOSITORY"
