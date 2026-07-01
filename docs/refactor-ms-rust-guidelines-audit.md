# Rust Guidelines Audit and Refactoring Plan

Date: 2026-06-26
Branch: `refactor/ms-rust-guidelines`
Scope: repository-wide audit using `/home/zajca/Code/me/ms-rust-skill/SKILL.md`

## Assumptions

- This pass is an audit and refactoring proposal only. It intentionally does not
  change Rust behavior.
- The current single-operator trust boundary stays in place for now: local Unix
  socket permissions plus NetBird/WireGuard for remote access.
- The project is still pre-1.0, so some public-library guidelines can be staged
  rather than applied everywhere immediately. They should be applied strictly to
  any extracted SDK/client crate.
- The repository's own patterns and current CI gates remain the default
  implementation style.

## Success Criteria

- The repository contains a written report describing what is wrong, what is
  needed, and how to refactor.
- Findings are prioritized and tied to concrete files or verification output.
- The plan preserves existing behavior first, then prepares for SDK and desktop
  work described in `docs/ROADMAP.md`.
- No placeholder implementations, code stubs, or behavioral shortcuts are
  introduced by this audit.

## Audit Inputs

- Loaded the Rust skill at `/home/zajca/Code/me/ms-rust-skill/SKILL.md`.
- Applied the linked Rust guideline files, including universal, project,
  application, correctness, documentation, safety, performance, FFI, macro,
  library-building, interoperability, resilience, and UX guidance.
- Checked repository-local agent context. There is no root `AGENTS.md` or
  `CLAUDE.md`, but `.claude/agent-memory` contains stale project notes and
  useful security/silent-failure review history.
- Inspected the full repository inventory with `rg --files`, crate manifests,
  CI/release workflows, docs, scripts, and Rust source layout.
- Ran the verification commands listed at the end of this file.

## Executive Summary

The project is in better shape than its module sizes suggest. The static gates
are strong, the test suite is broad, unsafe code is localized and documented, and
the daemon has good fail-closed behavior around sockets, config, hooks, and
secret-bearing state.

The main problems are structural:

1. The docs gate currently fails because the knowledge source map references a
   path that no longer exists.
2. Release packaging advertises `README.md` and `LICENSE`, but the repository has
   neither at the root even though the workspace declares `license = "MIT"`.
3. Several core modules are too large for the next phase: session management,
   metadata storage, worktree orchestration, API dispatch, and CLI parsing.
4. Workspace dependency centralization is incomplete.
5. The planned SDK extraction is blocked by a CLI-owned client, Debug-derived
   string contracts, and protocol APIs that need public-client polish.
6. Roadmap and architecture docs disagree about the next UI direction.
7. Owner-private files are sometimes created first and chmodded afterward. This
   is low risk under private parent directories, but it is not the strongest
   owner-only creation pattern.

The right next move is not a large rewrite. First fix the failing docs gate and
release hygiene, then centralize manifests, then extract the client SDK behind
tests, and only then split the large daemon modules along existing boundaries.

## What Is Working Well

- CI already covers formatting, clippy with warnings denied, tests, release
  build, docs check, `cargo-audit`, feature-matrix checks with `cargo-hack`, and
  unused dependency checks with `cargo-udeps`.
- `cargo test --workspace --all-features` passes outside the sandbox with 798
  tests across 30 test suites.
- `cargo clippy --workspace --all-targets --all-features` reports no issues.
- `cargo fmt --all --check` passes.
- Most crates forbid or deny unsafe code. The remaining unsafe calls are
  localized FFI wrappers with `#[expect(unsafe_code)]` and safety comments.
- Config parsing uses typed structures and strict fields in sensitive areas.
- Hook execution uses cleared environments and explicit allowlists, matching the
  repository's no-secret persistence model.
- Structured error types are used consistently for protocol and daemon failures.
- The event log and metadata store are explicitly designed to avoid terminal
  bytes and secrets.
- The previous silent-failure class around assistant activity parsing appears
  fixed: parse failures now return/log errors instead of being silently dropped.

## Priority Findings

### P0: Docs Check Is Failing

`cargo xtask docs check` currently fails:

```text
[PASS] schema-validation: 19 files, 17 concept(s)
[PASS] deterministic-build: sha256:9fed5d44dc0f0fb27ac142c887d833a5a2d7ab3aa2946f3c05f6467877375814
[FAIL] source-map-paths: 1 missing path(s)
        crates/daemon/src/detect/manifest.rs
[PASS] runbook-commands: 81 command(s) parsed successfully
[PASS] secret-scan: no credential patterns found in 24 bundle file(s)
```

The broken reference is in `docs/knowledge/assistant/source-map.md:62`. The
actual code now lives under `crates/daemon/src/detect/manifest/mod.rs`.

Recommended fix:

- Update the source-map path to the real file path.
- Re-run `rtk cargo xtask docs check`.
- Treat this as a release blocker because release packaging includes the offline
  docs bundle.

### P0: Release Packages Lack Root README and LICENSE

The workspace declares `license = "MIT"` in `Cargo.toml:53`, but the repository
has no root `LICENSE`. It also has no root `README.md`.

The release workflow only copies those files if present:

```text
.github/workflows/release.yml:120  for extra in README.md LICENSE; do
.github/workflows/release.yml:121    [ -f "${extra}" ] && cp "${extra}" "${staging}/" || true
.github/workflows/release.yml:122  done
```

Recommended fix:

- Add a root `LICENSE` matching the declared MIT license.
- Add a root `README.md` with install, quick start, trust boundary, support
  status, and links into `docs/`.
- Add a lightweight CI or release check that fails when required release extras
  are missing, instead of silently omitting them.

### P1: Core Modules Are Too Large for the Next Refactor

The largest files are carrying multiple responsibilities:

- `crates/daemon/src/session/mod.rs`: 3921 lines.
- `crates/cli/src/lib.rs`: 1641 lines.
- `crates/daemon/src/worktree/mod.rs`: 1455 lines.
- `crates/daemon/src/store/mod.rs`: 1307 lines.
- `crates/daemon/src/api/handler.rs`: 994 lines.
- `crates/daemon/src/project/config.rs`: 972 lines.
- `crates/daemon/src/agent/profile.rs`: 891 lines.
- `crates/xtask/src/lib.rs`: 988 lines.
- `crates/xtask/src/eval.rs`: 754 lines.
- `crates/protocol/src/session.rs`: 575 lines.

This is now a maintainability issue, not just a style issue. The roadmap's
SDK and desktop work will need stable public seams, smaller review scopes, and
mockable process/filesystem boundaries.

Recommended refactor:

- Split by existing behavior boundaries, not by abstract layers.
- Keep pure helper unit tests near helpers, but move user-visible flows into
  integration tests under `tests/` where the public API can drive them.
- Do not combine this decomposition with behavior changes.

Suggested decomposition:

- `session/registry.rs`: `SessionRegistry` state and public facade.
- `session/create.rs`: launch, initial input, project/worktree resolution,
  rollback on failure.
- `session/lifecycle.rs`: stop, exit handling, resize, inspect.
- `session/persistence.rs`: resume bindings, frozen profiles, store reload.
- `session/projects.rs`: project enrichment and project removal effects.
- `store/model.rs`: resume, worktree, and project record types.
- `store/file.rs`: JSONL read/write, atomic replace, version compatibility.
- `store/projects.rs`, `store/resume.rs`, `store/worktrees.rs`: domain methods.
- `worktree/git.rs`: git command execution and ref validation.
- `worktree/hooks.rs`: hook execution and environment shaping.
- `worktree/cleanup.rs`: prune/remove lifecycle.
- `worktree/path.rs`: slugs, paths, and path safety.
- `api/session.rs`, `api/project.rs`, `api/host.rs`, `api/assistant.rs`:
  request handlers by protocol domain.
- `cli/args.rs`: clap types.
- `cli/dispatch.rs`: top-level command dispatch.

### P1: Workspace Dependency Centralization Is Incomplete

The root manifest centralizes many dependencies in `[workspace.dependencies]`
at `Cargo.toml:25`, but several crate manifests still define dependency versions
or path dependencies locally:

- `crates/daemon/Cargo.toml:14-17` uses internal path dependencies directly.
- `crates/daemon/Cargo.toml:33` pins `portable-pty = "0.9"` locally.
- `crates/xtask/Cargo.toml:14-15` uses internal path dependencies directly.
- `crates/xtask/Cargo.toml:21` pins `pulldown-cmark = "0.12"` locally.

Recommended fix:

- Move all internal crates to root `[workspace.dependencies]`.
- Move `portable-pty` and `pulldown-cmark` to root `[workspace.dependencies]`.
- Use `{ workspace = true }` in member crates.
- Keep package renames local only where the consuming crate needs a local alias,
  but source path and version should still be workspace-owned.

This reduces dependency drift before adding `crates/client` and any desktop app
crate.

### P1: SDK Extraction Needs Protocol and Client API Polish First

`docs/ROADMAP.md:196-203` says the next sequence is:

1. Extract the Rust SDK in `crates/client`.
2. Document public API and version negotiation.
3. Build the native desktop companion app on that SDK.
4. Add the browser control center later and optionally.

The current client is still in `crates/cli/src/client.rs`, and the CLI snapshot
code documents a contract hazard:

```text
crates/cli/src/commands/assistant/snapshot.rs:549
source/state/activity strings are derived with format!("{:?}", x).to_lowercase()
```

That couples JSON output to `Debug` formatting of protocol enums instead of a
canonical string representation.

Recommended fix before or during SDK extraction:

- Create `crates/client` with framed request/response transport, raw attach
  transport, timeout/cancellation policy, and typed request methods.
- Keep the CLI as a thin consumer of the SDK.
- Add protocol enum helpers such as `as_str()` or stable string newtypes for
  `ProjectSource`, `SessionState`, `AgentActivity`, `DoctorStatus`,
  `ProviderKind`, and similar client-visible enums.
- Replace Debug-derived string contracts in CLI snapshot code with protocol-owned
  helpers.
- Document version negotiation in rustdoc and the user docs.
- Add SDK integration tests against loopback Unix and TCP fixtures.

### P1: Public API Documentation Is Good, but Not Yet SDK-Ready

The protocol crate exposes many public items through root re-exports in
`crates/protocol/src/lib.rs:33-60`, but the re-exports are not marked with
`#[doc(inline)]`.

The Rust skill asks public re-exports to be documented inline and public APIs to
be documented as a stable contract. The current code is reasonable for an
internal protocol crate, but Track S turns parts of this surface into an SDK
contract.

Recommended fix:

- Add `#[doc(inline)]` to public re-exports in `pohunek-protocol`,
  `pohunek-knowledge`, and any future SDK crate.
- Re-enable stricter public documentation lints for `pohunek-protocol` and
  `pohunek-client` instead of relying only on the workspace-wide allows at
  `Cargo.toml:107-112`.
- Add doc examples for common client workflows: health, session list, session
  create, attach, project actions, and version negotiation.
- Keep a single public path per item. The current private module layout in
  `pohunek-protocol` already supports this.

### P1: Roadmap and Architecture Docs Disagree

`docs/README.md:27-29` still says the eventual GUI is a browser control center
served by a standalone TypeScript aggregator backend. `docs/ROADMAP.md:164-175`
keeps the browser control center as later/optional, while `docs/ROADMAP.md:196-203`
now makes the Rust SDK and native desktop companion app the primary next path.

There are also stale references to a missing `NEXT.md`:

- `docs/ROADMAP.md:28-30` says `NEXT.md` is stale, but the file is not present.
- `docs/design/per-project-actions-and-worktree-hooks.md` references
  `../../NEXT.md`.
- `docs/design/per-project-actions-and-worktree-hooks-plan.md` references
  `../../NEXT.md`.
- `crates/daemon/src/lib.rs:13` and `crates/daemon/src/store/mod.rs:11` mention
  `NEXT.md`.

Recommended fix:

- Decide whether `NEXT.md` should be restored as a short current planning file or
  remove/replace all references with `docs/ROADMAP.md`.
- Update `docs/README.md` and `docs/architecture.md` to match the current path:
  SDK first, native desktop next, browser later/optional.
- Keep old phase/design docs explicitly historical if they intentionally preserve
  superseded plans.
- Clean up `.claude/agent-memory` separately or mark the old `zagentmesh` notes
  as stale so future agents do not treat them as current architecture.

### P2: Owner-Private Files Should Be Created With Private Mode Initially

Two owner-private writes create/open files first and chmod afterward:

```text
crates/daemon/src/store/mod.rs:704  fn write_owner_private(...)
crates/daemon/src/store/mod.rs:705      fs::write(path, bytes)?;
crates/daemon/src/store/mod.rs:709      fs::set_permissions(path, ... 0o600)?;

crates/daemon/src/events/mod.rs:60   OpenOptions::new().create(true).append(true).open(&path)?;
crates/daemon/src/events/mod.rs:64   fs::set_permissions(&path, ... 0o600)?;
```

The parent directories are intended to be owner-private, so this is likely low
risk in normal operation. Still, the stronger Unix pattern is to create files
with the final mode from the beginning.

Recommended fix:

- Use `std::os::unix::fs::OpenOptionsExt::mode(0o600)` for Unix creation paths.
- Keep the chmod fallback for already-existing files.
- Add tests that owner-private metadata/event files are created as `0600`.
- Consider applying the same review to generated setup scripts and assistant
  snapshots where appropriate, based on whether they may contain sensitive paths
  or prompt context.

### P2: App-Level Configuration Is Mostly Good, but Some Defaults Deserve Policy Review

The Rust skill warns against silent defaults for required configuration. The
daemon currently has several reasonable application defaults, such as shell
fallbacks and default attach token TTL. These are acceptable for a personal
daemon, but SDK and desktop clients will make the policy more visible.

Recommended fix:

- Keep ergonomic defaults for non-security settings.
- Make security-relevant values explicit in config or documented constants:
  attach token TTL, remote port, bind address validation, log directory, and
  runtime directory permissions.
- For public SDK APIs, avoid hidden global environment lookups where possible.
  Accept explicit paths/config structs and keep environment discovery in CLI/app
  layers.

### P2: Testing Is Broad but Too Concentrated in Large Modules

Many important tests live inside large implementation modules, especially
`crates/daemon/src/session/mod.rs`, `crates/daemon/src/store/mod.rs`, and
`crates/cli/src/lib.rs`. This makes future refactors harder because moving code
also moves large test blocks.

Recommended fix:

- Move user-visible behavior tests into crate-level `tests/` integration suites.
- Keep pure parsing, small helper, and invariant tests near the code they cover.
- Add contract tests for any new SDK crate that use only public API.
- Add focused regression tests before splitting modules. The goal is a behavior
  harness that stays green while files are moved.

### P2: Performance Work Should Wait for Benchmarks

The Rust skill allows application crates to choose fast allocators and CPU tuning
where appropriate. This project is I/O and PTY heavy, so allocator or target-cpu
changes should not be made speculatively.

Recommended fix:

- Add benchmarks or measurement commands for the real hot paths first:
  session list/inspect, event replay, attach relay, project action rendering,
  source-map/docs build, and worktree creation.
- Only then evaluate `mimalloc`, release profile tweaks, or future-size boxing in
  large async paths.
- Keep daemon release builds debuggable unless measurements show a clear need.

## Refactoring Roadmap

### Phase 0: Unblock Current Gates

Goal: make the repository releasable again without touching behavior.

- Fix the broken source-map path.
- Add root `README.md`.
- Add root `LICENSE`.
- Align `docs/README.md`, `docs/architecture.md`, and `docs/ROADMAP.md`.
- Decide the fate of `NEXT.md` references.
- Re-run `rtk cargo xtask docs check`, `rtk cargo test --workspace --all-features`,
  `rtk cargo clippy --workspace --all-targets --all-features`, and
  `rtk cargo fmt --all --check`.

### Phase 1: Manifest and Public-Crate Hygiene

Goal: reduce drift before adding new crates.

- Centralize all internal crates and external dependency versions in root
  `[workspace.dependencies]`.
- Add per-public-crate lint policy for docs and error docs.
- Add `#[doc(inline)]` to public re-exports.
- Do a small Rust 2024 edition migration spike. The workspace is currently
  `edition = "2021"` in `Cargo.toml:52`. Do not migrate blindly; confirm MSRV,
  dependency readiness, and clippy output first.

### Phase 2: Extract the Rust SDK

Goal: create the stable seam that native desktop and later browser work can use.

- Create `crates/client`.
- Move reusable pieces from `crates/cli/src/client.rs`.
- Add a transport abstraction for Unix socket, TCP framed RPC, and raw attach.
- Add typed methods over protocol methods.
- Add SDK error types with stable categories and source preservation.
- Add SDK examples and integration tests.
- Keep the CLI as a thin wrapper using the SDK.

### Phase 3: Split Protocol and API Dispatch by Domain

Goal: make request handling reviewable and ready for client evolution.

- Add protocol enum string helpers and replace Debug-derived string output.
- Split `crates/daemon/src/api/handler.rs` into domain modules.
- Keep method constants centralized in `pohunek-protocol`.
- Add compatibility tests that round-trip every request/response/event used by
  the CLI and SDK.

### Phase 4: Decompose Daemon State Modules

Goal: reduce blast radius without changing behavior.

- Split `session/mod.rs` after SDK tests are in place.
- Split `store/mod.rs` around file storage and domain records.
- Split `worktree/mod.rs` around git execution, hooks, cleanup, and path safety.
- Extract command/process runners where mocking helps tests avoid shelling out.

### Phase 5: Security and Performance Hardening

Goal: tighten owner-private guarantees and add measurement before optimization.

- Create owner-private files with `OpenOptionsExt::mode(0o600)`.
- Add explicit privacy policy for event payload paths and future SDK/desktop
  surfaces.
- Revisit remote TCP trust boundary before any multi-user, browser, or shared
  desktop surface.
- Add benchmarks for attach relay, event replay, store operations, docs build,
  and worktree flows.
- Evaluate allocator/profile changes only with benchmark evidence.

## Suggested Target Crate Layout

Near-term workspace:

```text
crates/
  client/       public Rust SDK over pohunek-protocol
  cli/          clap parsing, human output, SDK consumer
  daemon/       daemon runtime and API handlers
  protocol/     wire types, method constants, stable string repr helpers
  knowledge/    embedded docs bundle and materializer
  hostcheck/    host capability probes
  netbird/      NetBird parsing, host resolution, bind validation
  xtask/        docs and repository maintenance tasks
```

Possible daemon module layout:

```text
crates/daemon/src/
  api/
    mod.rs
    session.rs
    project.rs
    host.rs
    assistant.rs
  session/
    mod.rs
    registry.rs
    create.rs
    lifecycle.rs
    persistence.rs
    projects.rs
    attach.rs
    hooks.rs
    input.rs
    resume.rs
  store/
    mod.rs
    model.rs
    file.rs
    projects.rs
    resume.rs
    worktrees.rs
  worktree/
    mod.rs
    git.rs
    hooks.rs
    cleanup.rs
    path.rs
```

## Verification Performed

Commands were run from `/home/zajca/Code/me/zremoteng` on branch
`refactor/ms-rust-guidelines`.

Passed:

- `rtk cargo test --workspace --all-features`
  - The first sandboxed run failed because TCP bind tests received
    `PermissionDenied`.
  - The escalated rerun passed: 798 tests across 30 suites in 7.36s.
- `rtk cargo clippy --workspace --all-targets --all-features`
  - No issues found.
- `rtk cargo fmt --all --check`
  - Passed.
- Shell syntax checks for scripts:
  - `scripts/lib.sh`
  - `scripts/release`
  - `scripts/pohunek-launch-pr`
  - `scripts/pohunek-launch-issue`
  - `scripts/pohunek-rofi`
  - `scripts/pohunek-rofi-issue`

Failed:

- `rtk cargo xtask docs check`
  - Fails on one missing source-map path:
    `crates/daemon/src/detect/manifest.rs`.

Not run:

- Network-backed advisory or registry refresh commands beyond the existing CI
  workflow definitions.
- Miri. The repository has localized unsafe FFI, but this audit did not attempt
  a Miri pass.
- Benchmarks. No benchmark suite was found during this audit.

## Recommended Next Work Order

1. Fix the docs check failure and add root release files.
2. Synchronize roadmap/architecture docs and remove or restore `NEXT.md`.
3. Centralize all workspace dependencies.
4. Add protocol string helpers and replace Debug-derived snapshot strings.
5. Extract `crates/client` with contract tests.
6. Split API handlers by domain.
7. Split daemon session, store, and worktree modules.
8. Tighten owner-private file creation.
9. Add benchmarks before performance tuning.

## Non-Goals of This Audit

- No Rust code was refactored.
- No public API shape was finalized.
- No SDK crate was created.
- No release files were added.
- No docs source-map fix was applied.
