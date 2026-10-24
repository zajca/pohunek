# Coding

- **Rust guidelines are mandatory.** This repo follows the Microsoft Pragmatic
  Rust Guidelines, vendored in-repo at **`.agents/rust-guidelines/`**. Before
  writing or modifying any `.rs` file, read the files that apply to your task
  from that directory and apply them; `.agents/rust-guidelines/SKILL.md` is the
  index of which file to read when. Start with `11_universal_guidelines.md` (all
  Rust work); add `02_application_*` (CLI/desktop, error handling), `03_correctness_*`,
  `06`/`12`/`13`/`14`/`15` (library design) as the task warrants. Key points:
  `M-CANONICAL-DOCS` doc format, short names, documented magic values,
  `#[expect(..., reason = "...")]` over `#[allow]`. Files that fully comply carry
  a `// Rust guideline compliant <date>` marker — keep it accurate when you edit.
- **Errors:** typed errors with `thiserror` per crate (`CoreError`, `GitHubError`,
  `ConfigError`, …). Handle specific variants; avoid bare catch-alls. Library
  crates set `#![forbid(unsafe_code)]`; binaries deny with localized opt-in.
- **Config fails fast.** Validate required config at load and return a typed
  error; do not invent silent defaults for required values (sensible documented
  platform defaults like `notify-send` are fine, as named constants).
- **No hardcoded magic values.** Use named constants with a rationale comment.
- **Secrets never enter code, logs, errors, or agent context.** Linear tokens are
  read per-call from the keyring; `gh` output is redacted before it enters an
  error; types holding secrets get hand-written redacting `Debug`. Never read
  `.env*`, key/cert files, or print token values. Keep this posture.
- **Shared logic goes in a library crate, not a binary.**
- **Tests follow the [testing policy](verification.md#testing-policy).** A test drives cooperating
  production components through a supported boundary (integration) or the real
  product processes (E2E); never a new unit test. File placement (`tests/` vs
  inline `#[cfg(test)]`) follows the boundary a scenario exercises, not a
  unit/component tier, and a test does not expose a private API only to move
  it (see the linked testing policy for the migration state of existing unit-tier
  tests). Extending the protocol or state-machine suites is still the first
  option for behavior they own, but an added scenario must itself meet the
  integration/E2E boundary — appending to an existing suite does not exempt a
  test from it, and never adds unit-tier tests to those suites.
- **Wire protocol changes ripple.** Keep changes scoped. If you touch the wire
  protocol (`crates/protocol`), expect ripples in `client`, `daemon`, and `cli`,
  and into `docs/public-api.md` and the `docs/knowledge/` bundle — update all of
  them and run their checks. A non-additive wire change (rename, moved field)
  bumps `PROTOCOL_VERSION`, adds the edge adapter for the version it leaves
  behind in `crates/protocol/src/compat/` with golden fixtures and consumer
  recordings produced by that release's own code, and deletes the oldest adapter
  in the same change. A semantic change (new required parameter, removed method
  or event, changed error code or meaning) cannot be adapted: it also raises
  `MIN_PROTOCOL_VERSION` and is announced as a break. A method added after the
  previous release is listed in the adapter's `INTRODUCED_METHODS` so older
  connections get `method_not_found`.
- **Keep the assistant knowledge bundle current.** `docs/knowledge/` is the
  hand-authored source for the Universal Pohunek Assistant (materialized via
  `assistant.materialize`). Whenever a change alters something the bundle
  describes — a CLI command or flag, a protocol method/event, an
  operating-model concept (sessions/projects/worktrees/agent profiles), a safety
  rule, the public-API surface in `docs/public-api.md`, or a path listed in
  `docs/knowledge/assistant/source-map.md` — update the matching knowledge
  file(s) in the *same* change and re-run `cargo xtask docs check`. A stale
  bundle is treated like stale code, not a follow-up. If unsure whether a change
  is assistant-visible, check whether any file under `docs/knowledge/` mentions
  the surface you changed.
- **Record the knowledge-log entry as a fragment.** A change adds its entry as
  `docs/knowledge/log.d/<issue>-<slug>.md` and never edits
  `docs/knowledge/log.md`: one heading line
  `## Unreleased (<YYYY-MM-DD>, <topic>, #<issue>)` citing the issue of the file
  name, then the body, with links relative to `docs/knowledge/`. Independent
  changes then never conflict on the log. The shipped bundle shows the fragments
  as unreleased sections, `scripts/release` files them under the release
  heading, and `cargo xtask docs check` refuses an unreleased section in
  `log.md`. A branch that still edits `log.md` converts its sections with
  `cargo xtask docs log migrate`.
- **Keep shell completion synchronized with the CLI.** The clap command tree is
  the source of truth for generated Bash, Zsh, and Fish completion; never
  hand-maintain generated shell scripts. Every command, subcommand, flag, or
  argument change must also review `crates/cli/src/completion.rs`, update any
  affected dynamic value completers (especially `--host` and session `target`
  arguments), and keep the completion scenarios current where the change adds
  or alters completable behavior (see the linked testing policy). Preserve the completion
  safety contract: static generation performs no daemon or overlay I/O, while
  dynamic lookups are opt-in, deadline-bounded, do not autostart the daemon, and
  fail silently. In the same change, update the CLI tables/examples in
  `docs/cli.md`, matching `docs/knowledge/` guidance and source map entries, and
  release/install coverage when those surfaces change. Run at least
  `cargo test -p pohunek-cli` and `cargo xtask docs check` before the full gates.
- Comments and all repository text are in **English**.

The first complete team-relay release intentionally trusts collaborators at the
host Unix-account boundary. Relay API ACLs are authorization controls, not
command or hostile-workload isolation: direct-host profiles run under the
daemon owner's account. Real profile-backed container and VM isolation is a
post-release track owned by
[#88](https://github.com/zajca/pohunek/issues/88); do not call the relay track a
PoC or imply that current direct-host execution is a hostile-workload sandbox.
