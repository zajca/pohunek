# Architecture Review Follow-up: Remaining Work (2026-07-04)

This document tracks the larger architecture-review items that remain after the
2026-07-04 fixup branch. It is intentionally not a completion report: the branch
addressed several concrete correctness, protocol, path, and tooling findings,
but some review items are still structural work that should be planned as
separate changes.

## Status After The Fixup Branch

The current branch has already addressed these review areas:

- Shared path handling moved into `pohunek-paths`, with CLI, daemon, GUI, and
  gui-core using the shared XDG/environment contract.
- Protocol typed-method infrastructure exists in `pohunek-protocol`, and the
  SDK has typed `Client::call`, handshake, and centralized request-id helpers.
- Daemon resume stop handling now removes stale in-memory resume bindings before
  persistence, covering the observed stop/resume consistency bug.
- Worktree git commands have bounded process execution with process-group kill
  behavior.
- Blocking resume-store filesystem work is moved behind blocking tasks where it
  can stall the async runtime.
- Codex config editing now uses TOML-aware mutation instead of ad hoc string
  rewriting.
- Dead agent resume adapter code was removed.
- Host doctor checks are shared between local hostcheck and daemon doctor paths.
- `xtask` was split into focused modules and deduplicated around shared
  generator/check helpers.
- GUI unknown-host async results are guarded so stale results do not mutate
  unrelated host state.

The following sections describe the work that still remains.

## Remaining Work

### Daemon Session And Handler Decomposition

`SessionRegistryInner` still owns too many unrelated concerns. The branch fixed
a specific resume-binding bug, but the registry remains a broad coordination
object for session state, lifecycle, attach streams, event subscriptions,
configuration, runtime handles, and persistence.

Acceptance criteria:

- Split registry-owned state into focused owners or helper modules with narrow
  APIs, for example session map/lifecycle, attach streams, event subscriptions,
  persistence, and runtime/config handles.
- Keep the registry as orchestration glue rather than a container for every
  daemon session concern.
- Preserve existing session behavior with targeted unit tests for lifecycle,
  attach, subscription, and resume-binding paths.

`api/handler.rs` also still mixes unrelated method families. Typed protocol
methods make a cleaner split possible, but the handler has not yet been divided
by domain.

Acceptance criteria:

- Split daemon request handling into domain modules such as daemon/health,
  host, session, project, integration, assistant, and worktree.
- Keep shared request parsing, response construction, and error mapping in common
  helpers.
- Migrate tests to assert behavior at the domain boundary instead of relying on
  one very large handler module.

Remaining large methods and localized lint suppressions should be revisited only
after these decompositions. The goal is to remove real complexity, not to shuffle
large functions into equally broad modules.

### GUI Core Message And Module Split

`gui-core::Message` still conflates UI intents with asynchronous/domain results.
The current branch fixed compile-time issues and added a guard for stale
unknown-host results, but it did not change the core message model.

Acceptance criteria:

- Split UI intent messages from asynchronous results and domain events.
- Keep the state transition layer in `gui-core` headless and testable.
- Ensure stale async results are ignored consistently without blocking legitimate
  UI intent paths that intentionally create host state.

`gui-core/src/lib.rs` and `gui/src/main.rs` remain large modules. They should be
split after the message model is clarified.

Acceptance criteria:

- Extract gui-core modules such as `message`, `state`, `providers`, `commands`,
  and focused feature modules where appropriate.
- Keep `gui` as a thin Iced shell around gui-core state, commands, and
  subscriptions.
- Add focused tests around assistant materialization, degraded states, remote
  behavior, and stale-result handling.

Extraction of a deeper domain/control core should wait until the message split
settles. Otherwise the extracted API will likely preserve the same conflated
message boundaries.

### Protocol And SDK Callsite Migration

Typed protocol-method infrastructure now exists, but not every normal
CLI/gui-core callsite uses it yet. Some code still constructs raw `Request`
values or manually deserializes `serde_json::Value` responses.

Acceptance criteria:

- Migrate ordinary request/response methods to
  `Client::call::<protocol::method::...>()`.
- Keep the low-level `request` API only for attach, subscribe, transport, and
  protocol-framing tests where raw protocol access is intentional.
- Route subscribe request IDs through `Client::next_request_id` or a typed
  subscribe helper consistently across CLI and gui-core.
- Update public API and knowledge documentation if the exposed SDK surface
  changes further.

### Naming And Module Cleanup

Several naming collisions and misplaced helpers remain. These are lower-risk
than the daemon and GUI refactors, but they still make the codebase harder to
navigate.

Acceptance criteria:

- Resolve the two `detect` concepts: activity/session detection and project
  repository detection should not share the same module name.
- Move bounded/shared git utilities out of `project::detect` if they are reused
  outside project identity detection.
- Rename or move `integration` hook-install code to make it clear that it
  installs agent hooks rather than owning provider integrations.
- Gather shared validation primitives, especially leading-dash and name/ref
  validation, without erasing the distinct boundaries between session, project,
  and git-reference validation.
- Consider lifting `project/config.rs` to a top-level config/project-config
  module if it continues to be independent from project detection.
- Deduplicate git porcelain parsers so project detection can reuse the fuller
  parser already used by project display paths.

### Xtask And Documentation Follow-up

The current branch split `xtask` and deduplicated common generator/check logic.
Future changes should keep the new boundaries from regressing.

Acceptance criteria:

- Add focused tests for Clap dispatch, documentation generator invariants, and
  docs-check wiring when those areas change again.
- Keep generated documentation, `docs/public-api.md`, and
  `docs/knowledge/assistant/source-map.md` aligned with any future CLI,
  protocol, GUI, or knowledge-source changes.

### Path And Environment Cleanup

Shared path handling is in place through `pohunek-paths`. Future cleanup should
be opportunistic rather than a separate broad rewrite unless a concrete mismatch
is found.

Acceptance criteria:

- Remove any remaining duplicated path constants only when they are discovered
  in touched code.
- Keep required configuration fail-fast and documented.
- Consider documenting a non-tmpfs target directory for large local validation
  runs if the development environment continues to fill `/tmp`.

## Suggested Execution Order

1. Finish the GUI message split before extracting more GUI modules.
2. Decompose `SessionRegistryInner` around concrete state ownership.
3. Split daemon API handler domains using the typed protocol method layer.
4. Migrate remaining SDK callsites from raw requests to typed methods.
5. Apply naming/module cleanup where it falls out of the structural work.
6. Add targeted xtask and documentation-invariant tests when those files change.

## Verification Expectations

Each future change should run the relevant narrow tests during development. A
branch that claims to finish any architecture-review item should also run the
full repository gate set before completion:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
cargo build --workspace --release
cargo xtask docs check
```

Some daemon and transport tests bind Unix or TCP sockets. In restricted agent
environments, those tests may need to run outside the sandbox to exercise the
same behavior as CI.
