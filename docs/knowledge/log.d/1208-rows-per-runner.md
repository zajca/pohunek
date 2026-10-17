## Unreleased (2026-10-16, release evidence rows run in one job per runner, #1208)

- The release evidence workflow runs its compatibility rows in one job per
  runner, `Rows on ubuntu-latest` and `Rows on macos-15`, instead of one job per
  (runtime x target) row plus a separate consumer build per OS. Each job builds
  the release consumer and xtask once and runs its rows in sequence through
  `scripts/release-workflow/run-rows`. The release path (`release.yml`) uses the
  same workflow.
- Each row runs in a fresh, owner-only root. After each row `run-rows` fails it
  for a process, root, user service, launchd job or keychain item it left
  behind. It stops the job when the row changed the account or anything a later
  row trusts, or when the guards could not check it; any other failing row does
  not stop the next one. A row over its 15-minute limit has its process group
  killed.
- The attestation and JUnit file names and the `attestation-*` pattern are
  unchanged. The artifacts are per runner: `attestation-<os>`,
  `matrix-junit-<os>` and the new `row-results-<os>`, from which the evidence
  verdict names every planned row that did not pass or sent no result. See
  [the release bundle](concepts/release-bundle.md).
