---
name: release
description: >-
  Cut a pohunek release with scripts/release — bump the workspace version, tag
  vX.Y.Z, push, and verify the GitHub Actions Release workflow builds and
  publishes the glibc + MUSL x86_64 binaries. Use when the user asks to cut
  or publish a new version.
---

# release — cut and publish a release

Wraps `scripts/release` and the `Release` GitHub Actions workflow
(`.github/workflows/release.yml`). Do the version bump through the script — never
hand-edit `Cargo.toml`/`Cargo.lock` versions or hand-craft the tag.

## Preconditions (the script enforces these; check them first)

- On the release branch — `main` by default (override with
  `POHUNEK_RELEASE_BRANCH`). The script aborts if you are on another branch.
- Working tree is clean. Commit or stash first; the script aborts on a dirty
  tree.
- The gates pass on the commit being released. The Release workflow re-runs
  fmt/clippy/test + `docs check` on the tag, so a red commit publishes nothing —
  run the `gates` skill before tagging to fail fast locally.
- The target tag does not already exist.

## Steps

1. **Compare the compatibility constants.** The script diffs these constants
   between the previous tag and `HEAD`: `STORE_SCHEMA_VERSION`
   (`crates/daemon/src/store/`), `PROTOCOL_VERSION` and `MIN_PROTOCOL_VERSION`
   (`crates/protocol/src/version.rs`), the worker protocol `CURRENT_VERSION` and
   `PREVIOUS_VERSION` (`crates/worker-protocol/src/version.rs`),
   `WORKER_JOURNAL_SCHEMA_VERSION` (`crates/daemon/src/runtime/lifecycle.rs`),
   and `EXPECTED_INTEGRATION_VERSION` (`crates/protocol/src/integration.rs`).
   Each one that changed alters the upgrade contract in `AGENTS.md` ("Upgrade
   window"), so the release needs a notes file with one line naming each changed
   constant and what an operator must do. Write it before the dry run and pass
   it as `--notes FILE`; the script aborts when a changed constant has no line.

2. **Confirm the bump.** Determine `patch`, `minor`, `major`, or an explicit
   `X.Y.Z` from the user's request ("minor release" → `minor`).

3. **Dry-run first.** Show what will happen without changing anything:

   ```bash
   scripts/release <patch|minor|major|X.Y.Z> [--notes FILE] --dry-run
   ```

   This prints the current and next version, the tag, and every changed
   compatibility constant with its old and new value. Confirm it matches
   intent.

4. **Cut the release.** Run the real thing:

   ```bash
   scripts/release <patch|minor|major|X.Y.Z> [--notes FILE]
   ```

   The script bumps `version` in `[workspace.package]` in `Cargo.toml`, refreshes
   `Cargo.lock`, commits `Release vX.Y.Z`, creates an annotated tag `vX.Y.Z`, and
   pushes both the branch and the tag to `origin`. Use `--no-push` if the user
   wants to inspect the commit/tag locally before pushing (then push the branch
   and tag manually as the script prints).

5. **Publish the notes.** When a notes file was needed, attach it to the GitHub
   Release once the workflow has created it (`gh release edit "vX.Y.Z"
   --notes-file FILE`) and confirm with `gh release view "vX.Y.Z"`.

6. **Record the release.** Via the `github-workflow` skill, comment the
   version/tag and release URL on any issue whose delivery this release
   completes (or opens a dedicated tracking issue beforehand when the release
   itself is planned work), and update the project status accordingly. Verify
   the writes from the API response.

7. **Verify the Release workflow — do not trust, confirm.** Pushing the `vX.Y.Z`
   tag triggers `.github/workflows/release.yml`, which runs the fmt/clippy/test
   gate + docs-gate, then builds `pohunek` and `pohunekd` for
   both `x86_64-unknown-linux-gnu` (dynamic glibc, primary) and
   `x86_64-unknown-linux-musl` (fully static, runs on any x86_64 Linux),
   packages per-component tarballs with sha256 checksums in read-only `build`
   jobs, and the `publish` jobs (the only Linux jobs with a write token, no
   checkout) download each archive, check its checksum, and attach it to the
   GitHub Release. After `sdk-gate` passes, `sdk-pack` builds (read-only token) and
   `sdk-publish` attaches the TypeScript SDK tarballs `pohunek-ts-{protocol,sdk,testkit}-X.Y.Z.tgz`, each
   with a `.sha256`. Watch it to completion:

   ```bash
   gh run watch "$(gh run list --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
   gh release view "vX.Y.Z"   # confirm the tarballs + .sha256 (incl. the three pohunek-ts-*.tgz) are attached
   ```

8. **Report.** State the published version/tag, the workflow conclusion
   (success/failure with the failing job if any), and the attached artifacts. If
   the workflow failed, report why with output — a failed gate means no binary
   was published.

## Constraints

- Never publish from a red commit; the workflow will block it, so catch it
  locally.
- Do not hand-edit versions or push a tag outside `scripts/release`.
- Verify the release actually published; never report "released" from the tag
  push alone without checking the workflow and the GitHub Release.
