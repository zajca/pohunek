---
name: release
description: >-
  Cut a pohunek release with scripts/release — bump the workspace version, tag
  vX.Y.Z, push, verify the GitHub Actions Release workflow builds and
  publishes the glibc + MUSL x86_64 binaries, publish the reviewed release
  post, and drive the downstream updates. Use when the user asks to cut or
  publish a new version.
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
  fmt/clippy/test (the nextest shards CI runs, plus doctests) + `docs check` on
  the tag, so a red commit publishes nothing — run the `gates` skill before
  tagging to fail fast locally.
- The target tag does not already exist.

## Steps

1. **Compare the compatibility constants.** The script diffs these constants
   between the previous tag and `HEAD`: `STORE_SCHEMA_VERSION`
   (`crates/daemon/src/store/`), `PROTOCOL_VERSION` and `MIN_PROTOCOL_VERSION`
   (`crates/protocol/src/version.rs`), the worker protocol `CURRENT_VERSION` and
   `PREVIOUS_VERSION` (`crates/worker-protocol/src/version.rs`),
   `WORKER_JOURNAL_SCHEMA_VERSION` (`crates/daemon/src/runtime/lifecycle.rs`),
   and `EXPECTED_INTEGRATION_VERSION` (`crates/protocol/src/integration.rs`).
   Each one that changed alters the upgrade contract in
   `.agents/instructions/project.md` ("Upgrade
   window"), so the release needs a notes file with one line naming each changed
   constant and what an operator must do. Write it before the dry run and pass
   it as `--notes FILE`; the script aborts when a changed constant has no line.

2. **Confirm the bump.** Determine `patch`, `minor`, `major`, or an explicit
   `X.Y.Z` from the user's request ("minor release" → `minor`).

3. **Dry-run first.** Show what will happen without changing anything:

   ```bash
   scripts/release <patch|minor|major|X.Y.Z> [--notes FILE] --dry-run
   ```

   This prints the current and next version, the tag, the knowledge-log
   fragments the release would file, the folded `CHANGELOG.md` section that
   becomes the release body, and whether the release notes cover any changed
   compatibility constants. It also describes the commit, evidence checks, and
   pushes it would perform. Confirm that matches intent.

4. **Cut the release.** Run the real thing:

   ```bash
   scripts/release <patch|minor|major|X.Y.Z> [--notes FILE]
   ```

   The script files the `docs/knowledge/log.d/` fragments under `## X.Y.Z` in
   `docs/knowledge/log.md` and removes them (`cargo xtask docs log fold`),
   folds the `changelog.d/` fragments into a `## X.Y.Z - <UTC date>` section of
   the committed `CHANGELOG.md` and removes them (`scripts/changelog fold`),
   bumps `version` in `[workspace.package]` in `Cargo.toml`, refreshes
   `Cargo.lock`, commits all of it as `Release vX.Y.Z`, and requires successful main and
   nightly CI evidence for that commit before creating an annotated tag
   `vX.Y.Z`. Evidence is a run on the commit itself or a `workflow_dispatch`
   run that tested the same git tree; `scripts/ci-evidence` prints which run
   matched and whether by `sha` or by `tree`. A `main` push reuses the newest
   green same-tree legs while `CI_REUSE_PUSH` is `true` (`scripts/ci-reuse`);
   the evidence union names every per-job source run (`run_ids`). By default it pushes the release
   branch before waiting for evidence and pushes the tag after verification.
   With `--no-push`, it checks only already-existing evidence and leaves the
   commit and tag local;
   it does not dispatch CI or push either ref. After local tagging, the script
   reminds you to push the release branch and tag manually. If the evidence
   check fails or times out, resolve the CI issue and rerun with the same
   explicit `X.Y.Z`; the workspace is already bumped, so do not request another
   relative bump such as `patch`.

5. **Publish the notes.** The compatibility notes of step 1 are published with
   the release post: step 8 puts them after the end marker and sets the whole
   body in one edit. Do not edit the body with the notes file alone.

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
   packages per-component tarballs with sha256 checksums in read-only build jobs
   (`release-build.yml`), builds the official runtime package archives, runs the
   compatibility rows, assembles and signs the runtime catalog and rebuilds the
   daemon archives with a `runtime/` directory (`release-evidence.yml`), runs the
   network-isolated archive smoke, and only then the single `publish` job (the
   only job with a write token, no checkout) verifies the complete inventory,
   creates a draft release, uploads and checks it, and publishes it. The
   TypeScript SDK tarballs `pohunek-ts-{protocol,sdk,testkit}-X.Y.Z.tgz`, each
   with a `.sha256`, are part of the same inventory. Watch it to completion:

   ```bash
   gh run watch "$(gh run list --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
   gh release view "vX.Y.Z"   # confirm the tarballs + .sha256 (incl. the three pohunek-ts-*.tgz) are attached
   ```

8. **Release post (required).** pohunek.dev shows only the part of the release
   body between `<!-- pohunek-changelog:start -->` and
   `<!-- pohunek-changelog:end -->` (ecosystem changelog contract, #1588), so
   that block is a post written for people, with the folded changelog section
   under it. Every release gets one, a patch release included.

   - **Draft.** As soon as step 4 has made the `Release vX.Y.Z` commit (the
     script then waits for CI evidence, which leaves time to write and review),
     print the skeleton from the release tree; before that, write from the
     section the step 3 dry run prints:

     ```bash
     POST=$(mktemp -t pohunek-vX.Y.Z-post.XXXXXX)   # outside the checkout; keep the path
     scripts/changelog notes X.Y.Z --post > "$POST"
     ```

     The file stays outside the release checkout: an untracked file there makes
     a rerun of `scripts/release` (step 4) refuse the dirty tree.

     It holds the markers, the parts to write (each carries
     `TODO(release-post)`) and `### Full changelog` with the folded section, its
     kind headings moved one level down. Do not edit the full changelog; a wrong
     entry is fixed in `CHANGELOG.md` of a later release.
   - **Write** to this shape (the prose is an example of the tone, not text to
     reuse):

     ```markdown
     <!-- pohunek-changelog:start -->
     Two or three sentences: what this release is about and who notices it.

     ### What's new

     **Sessions keep their profile when you resume them.** You can stop a
     session and pick it up later with the same agent and options, so long work
     survives a reboot without setting it up again.

     ### Upgrading

     Restart the daemon after installing; existing sessions are kept. Nothing
     else to do.

     ### Full changelog

     #### Added
     ...
     <!-- pohunek-changelog:end -->

     ## Compatibility notes

     One line per changed compatibility constant (the step 1 notes file);
     no section when none changed.
     ```

     Style: each highlight says what a user can now do and why it matters, in
     plain words, naming only the commands, flags and settings a user types.
     No internal jargon: no crate, module, constant, job or test names, no
     agent or CI process terms, no issue numbers in the prose (the full
     changelog links them). Two to five highlights for a minor release, one or
     two for a patch. Every breaking entry and every action an operator must
     take (restart, re-pair, package upgrade, changed compatibility constant)
     appears under `Upgrading`; write "Nothing to do" when that is true.
   - **After the end marker** goes only what the website must not show: the
     compatibility notes of step 1 (step 5 publishes them only through this
     body) and the assistant digest of step 9.
   - **Review.** Send the full body to the release coordinator or the owner and
     publish only after their explicit approval of that text. Before
     publishing, each check prints the value after it:

     ```bash
     grep -c 'TODO(release-post)' "$POST"                      # 0
     grep -cx '<!-- pohunek-changelog:start -->' "$POST"        # 1
     grep -cx '<!-- pohunek-changelog:end -->' "$POST"          # 1
     ```

   - **Publish** right after step 7 shows the release published. The Release
     workflow publishes the release without the markers, so the body has none
     from publication until this edit, and a site run inside that window
     records the release without the post (the website check below catches
     it). This edit sets the whole body, compatibility notes included (step 5).
     Confirm:

     ```bash
     gh release edit "vX.Y.Z" --notes-file "$POST"
     gh release view "vX.Y.Z" --json body --jq .body | grep -c 'TODO(release-post)'   # 0
     ```

   - **Website.** pohunek.dev `follow-release` (every 3 hours, or a manual
     dispatch, see step 10b.1; the core Release workflow does not notify it) records a
     release's body the first time it sees the release and keeps that record
     when the body changes later: the marked block as `source:
     "release-notes"`, or, for a body without markers, a list of pull request
     titles as `source: "generated"`. Check the recorded entry after the site
     has followed the release:

     ```bash
     gh api -H 'Accept: application/vnd.github.raw' \
       repos/pohunekd/pohunek.dev/contents/src/data/changelog/pohunek.json \
       | jq -r '.releases[] | select(.tag == "vX.Y.Z") | .source, .markdown' | head -4
     ```

     It prints `release-notes` and the post's intro. When the site recorded an
     earlier body, open a pohunek.dev pull request that sets that entry's
     `source` to `"release-notes"` and its `markdown` to the text between the
     markers of the published body, trimmed.
   - **X.com thread (required).** The owner announces each release on X.com.
     Draft the thread from the approved release post (the text between the
     markers) as sent for review, so the owner reviews both together; redraft
     it when the review changes the post. The skill never posts to X.com and never asks for X credentials:
     publishing is the owner's action.
     - **Shape.** English, 3 to 5 posts for every release, a patch included,
       each at most 280 characters. A URL counts as 23 characters
       whatever its length. The first post says what the release is and links
       `https://github.com/zajca/pohunek/releases/tag/vX.Y.Z`; the following
       posts each carry one highlight of the release post, in plain words and
       in the post's order; the last post may link `https://pohunek.dev`. Name
       only commands, flags and settings that the published release post names.
       Make no claim the release post does not make: no performance numbers,
       platform support or "now works everywhere" that the post does not state.
       Follow the post's `Upgrading` section when a highlight needs an operator
       action. No emoji, no hashtags.
     - **Plugin releases.** A plugin release that ships in the same cycle
       (`pohunekd/plugin-assistant` of step 9 included) is never part of the
       thread. Once its release is verified (attestation checked) and the
       plugin is admitted to the official plugin index, it gets a separate
       single-post file, checked like the thread. Until then it is only a note
       to the owner that it is not yet postable.
     - **Draft file.** Write the posts to a file outside the checkout, in order,
       separated by a line holding only `---`, with no numbering inside the
       text. The file holds posts only; notes for the owner (a pending plugin
       post, a link to confirm) go in the artifact or report next to it.
     - **Length check.** Print each post's length; every value must be at most
       280:

       ```bash
       X=$(mktemp -t pohunek-vX.Y.Z-x.XXXXXX)   # outside the checkout; keep the path
       python3 -I -c 'import re,sys; ps=open(sys.argv[1]).read().split("\n---\n"); print([(i,len(re.sub(r"https?://\S+","x"*23,p.strip()))) for i,p in enumerate(ps,1)])' "$X"
       ```

       A post over 280 is shortened, not split into more posts than the
       thread allows.
     - **Review.** Hand the draft to the owner: for a manager session a board
       artifact (`board.sh artifact`), otherwise the file path in the report of
       step 11. State that nothing was published. The thread is posted only
       after the release post is published and the website entry is checked.

9. **Release `pohunekd/plugin-assistant` from the new core tag.** The assistant
   knowledge bundle ships as the context package `pohunek.context.assistant`, and
   the plugin release workflow builds it with the `xtask` of a published core tag,
   so this step runs after the core release is verified. In a clone of
   `pohunekd/plugin-assistant`:

   ```bash
   scripts/assemble-bundle <checkout of the vX.Y.Z tag>   # refreshes package/bundle
   # package/runtime.toml: version = "X.Y.Z"
   # .github/workflows/release.yml: uses ...plugin-release.yml@vX.Y.Z and core-tag: vX.Y.Z
   git add -A package .github && git commit -m "Release X.Y.Z"   # new bundle pages are untracked: no `-a`
   test -z "$(git status --porcelain)"                           # nothing may stay out of the tag
   git push origin main
   git tag vX.Y.Z && git push origin vX.Y.Z
   gh run watch "$(gh run list --repo pohunekd/plugin-assistant --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
   gh release view vX.Y.Z --repo pohunekd/plugin-assistant   # archive + SHA256SUMS attached
   ```

   Verify the attestation (`gh attestation verify <archive> --repo
   pohunekd/plugin-assistant --signer-workflow
   zajca/pohunek/.github/workflows/plugin-release.yml`; the archive is signed by
   the core reusable workflow, so the bare form fails), then append the
   archive's sha256 to the core release notes of `zajca/pohunek` (these commands run in the plugin clone, so each names
   the repository): fetch the body first (`gh release view "vX.Y.Z" --repo
   zajca/pohunek --json body --jq .body > FILE`), add the digest line after the
   end marker, and
   `gh release edit "vX.Y.Z" --repo zajca/pohunek --notes-file FILE`, so the
   release post stays. The notes are where
   `docs/install.md` ("Install the assistant package") sends operators for the
   digest. Without this release, `pohunek assistant` answers
   `assistant_package_missing` on every host.

9b. **Admit the assistant to the official plugin index.** Each release needs
    two index pull requests in this order, because the core-range admission
    stage refuses a range that excludes `CORE_TAG` (see
    [Admission through the index](../../docs/knowledge/guides/plugin-repositories.md#admission-through-the-index)):

    1. A pull request moving `CORE_TAG` of `pohunekd/pohunek-plugin-index`
       to `vX.Y.Z`. Owner GO to merge; the merge publishes, because the sign
       job signs on merge.
    2. After it merges, a pull request adding
       `official/pohunek.context.assistant/X.Y.Z.toml` with
       `kind = "context"`, the release URL, the `SHA256SUMS` digest, both
       platforms, `core = ">=X.Y.0, <X.(Y+1).0"` and `[source]` with
       `built_with = "vX.Y.Z"`. Owner GO to merge.
    3. Verify that the next signed `official-index-v2.json` lists the entry
       (and that `official-index-v1.json` does not: schema 1 carries runtime
       entries only).
    4. The X.com single post for a plugin release (step 8) waits for that
       listing: it is drafted only after the entry is signed.

10. **Downstream updates (required).** Other repositories build on, pin or
    show the core release. For each consumer the release coordinator tells its
    owner (the repository's maintainer or agent session) the version, the
    changed compatibility constants of step 1 and which waiting work this
    release unblocks; each change goes through that repository's own pull
    requests and release process, and the coordinator tracks it to the done
    check.

    | Consumer | After a core tag | Done when |
    | --- | --- | --- |
    | `pohunekd/pohunek-companion` | A pull request moving `tag = "vX.Y.Z"` of `pohunek-client` and `pohunek-assistant` in `Cargo.toml` (with `Cargo.lock`), then the feature pull requests that waited for this release, then a companion release. | The companion release names core vX.Y.Z. |
    | `pohunekd/pohunek-companion-android` | The pin (`pohunek_protocol_tag` in `app/src/main/res/values/config.xml`, `POHUNEK_VERSION` and the sha256 args in `container/Containerfile`, the tag in `docs/architecture.md`) moves when the release changes `PROTOCOL_VERSION` or `MIN_PROTOCOL_VERSION`, or when the app needs a feature of it; then an app release. | The app release names core vX.Y.Z, or the coordinator records why the pin stays. |
    | `pohunekd/omarchy` | No pin: `tools/verified-download.bash` installs the newest core release. Check that what the widget reads (`pohunek session list --json`) stays compatible; change and release only when it does not. | The check result is recorded; a needed change is released. |
    | `pohunekd/plugin-assistant` | Step 9. | Its release vX.Y.Z is published and the digest is in the core notes. |
    | `pohunekd/pohunek-plugin-index` | Step 9b: the `CORE_TAG` PR, then the `pohunek.context.assistant/X.Y.Z` entry PR. | The signed `official-index-v2.json` lists the assistant entry. |
    | Hosts that run pohunek for other services (the `pohunekd/autoreview` review hosts) | Their operator gets the version and whether the protocol window requires an upgrade; upgrade on their schedule. | The operator acknowledged. |
    | pohunek.dev | `follow-release` moves the `pohunek` pin in `releases.config.json` and records the post (step 8); a manual run is `gh workflow run follow-release.yml --repo pohunekd/pohunek.dev -f product=pohunek`. The roadmap is hand-written and is reconciled in step 10b. | The site shows vX.Y.Z with the post, the roadmap names vX.Y.Z as released (step 10b), and each downstream release above once it is out. |

10b. **Website: changelog and roadmap.** Start with 10b.1 once step 8 has
    published the approved post (the Website check of step 8 needs it), because
    the site records a release body the first time it sees it. The core
    Release workflow does not notify the site.

    1. **Follow the release.** Dispatch the site's follow workflow for the new
       version and wait for it:

       ```bash
       START=$(date -u +%Y-%m-%dT%H:%M:%SZ)
       gh workflow run follow-release.yml --repo pohunekd/pohunek.dev -f product=pohunek -f version=X.Y.Z
       RUN=$(gh run list --repo pohunekd/pohunek.dev --workflow=follow-release.yml --event workflow_dispatch --json databaseId,createdAt --jq "map(select(.createdAt >= \"$START\")) | first | .databaseId")
       gh run watch --repo pohunekd/pohunek.dev --exit-status "$RUN"
       ```

       The run is picked by event and creation time, so an earlier scheduled
       run is never watched; rerun the `RUN=` line when it prints `null`, which
       means the dispatched run is not listed yet. Without `-f version` the
       run follows the newest release of the product named by `-f product`;
       without `-f product` it follows every product. A failed run is reported
       with its failing job; do not retry it blindly.
    2. **Verify the changelog page.** Re-query the live site, not the run:
       the recorded entry (`source` must be `release-notes`, see the Website
       check of step 8) and the rendered page:

       ```bash
       curl -fsS https://pohunek.dev/changelog/pohunek/ | grep -c 'id="pohunek-X.Y.Z"'   # 1
       ```

       The count stays 0 until the deploy that follows the run has finished;
       check the deploy run before reporting a missing page.
    3. **Update the roadmap.** `src/data/roadmap.ts` of `pohunekd/pohunek.dev`
       is hand-written. In one pull request there, move `roadmapRelease` to
       `vX.Y.Z` and `roadmapAsOf` to today, then reconcile against GitHub:
       - each track in `roadmapTracks`: its status, status note, progress and
         issue links follow the epic's state and child counts in
         `zajca/pohunek`; remove a track whose work is gone and add one for a
         new epic of the shipped milestone;
       - `nextMilestone`: the open milestone of the next release (number from
         its URL, title such as `0.46`, closed and total issue counts, and the
         epics that define it).

       The "Recent core releases" table of `/roadmap/` is derived from the
       recorded changelog and needs no edit; it shows vX.Y.Z without a roadmap
       update, so it proves nothing about this step. After the pull request is
       merged, check the file on `main`:

       ```bash
       gh api -H 'Accept: application/vnd.github.raw' \
         repos/pohunekd/pohunek.dev/contents/src/data/roadmap.ts \
         | grep -c "roadmapRelease = 'vX.Y.Z'"   # 1
       ```

       The roadmap page carries the change once the deploy that follows the
       merge has finished.

11. **Report.** State the published version/tag, the workflow conclusion
   (success/failure with the failing job if any), and the attached artifacts. If
   the workflow failed, report why with output — a failed gate means no binary
   was published. Include the release post URL and who approved it, the path or
   board artifact of the X.com thread draft (not posted), the
   plugin-assistant release and its digest, and the state of each downstream
   consumer of step 10.

## Constraints

- Never publish from a red commit; the workflow will block it, so catch it
  locally.
- Do not hand-edit versions or push a tag of this repository outside
  `scripts/release`. Step 9 edits and tags `pohunekd/plugin-assistant`, a
  separate repository; steps 10 and 10b change other repositories only through their
  own pull requests and releases.
- Never publish a release post that still holds `TODO(release-post)` or that
  the coordinator or owner has not approved.
- Never post to X.com and never ask for X credentials: step 8 only drafts the
  thread for the owner.
- Verify the release actually published; never report "released" from the tag
  push alone without checking the workflow and the GitHub Release.
