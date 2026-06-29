# GUI implementation review (`crates/gui` + `crates/gui-core`)

Date: 2026-06-29
Reviewer: code review pass against the MS-style Rust guidelines (`ms-rust-skill`)
and the project's own conventions.

## Scope

Files reviewed in full:

- `crates/gui/src/main.rs` (2830 lines) — Iced shell: `update`, `view`,
  command/task builders, config loading.
- `crates/gui/src/runtime.rs` (52 lines) — Tokio bridge for Iced.
- `crates/gui-core/src/lib.rs` (3115 lines) — headless `Workspace` state
  machine, SDK request helpers, reconnecting connection stream, `UiState`,
  link/launch domain types.
- `crates/gui-core/src/providers/github.rs` (683 lines) — `gh`-backed client.
- `crates/gui-core/src/providers/linear.rs` (624 lines) — GraphQL client.
- `crates/gui-core/src/providers/mod.rs`.
- Both `Cargo.toml` manifests and the workspace lint config.

## Overall assessment

The architecture is sound and the separation of concerns is the strongest part
of this codebase:

- **Headless core / view split is clean.** `gui-core` has zero Iced dependency;
  the Iced layer only wraps async helpers in `Task`/`Subscription`. This makes
  the state machine fully unit-testable, and the test suite (≈470 lines inline
  in `lib.rs` plus `tests/`) exercises the tricky parts: stale-request
  rejection, scope invalidation, notification de-duplication, backoff capping.
- **I/O is mockable.** `GhRunner`, `TokenSource`, and `GraphqlTransport` are
  trait ports with production and test implementations — matches the libraries
  resilience guideline (I/O is mockable, no hidden statics).
- **Secret hygiene is good.** Linear tokens are read per-call through the
  keyring and never persisted; `LinearClient`/`GitHubClient` have hand-written
  `Debug` that redacts; `gh` stderr is scrubbed for token prefixes and bearer
  tokens before it enters an error value. This aligns with the "secrets never
  enter context/logs" rule.

The main problems are **module size**, **mechanical duplication**, **one
inconsistency in error routing**, **a dual source of truth for selection**, and
**one real (if low-likelihood) shell-injection surface**. None are
correctness-critical for the happy path, but they are exactly the things that
make this code hard to evolve.

---

## High priority

### H1. `main.rs` and `lib.rs` are oversized single files

- `crates/gui/src/main.rs` — 2830 lines.
- `crates/gui-core/src/lib.rs` — 3115 lines.

Both mix several distinct responsibilities in one file. This is the dominant
maintainability issue and it amplifies every other finding below (duplication is
harder to see, `update`/`apply` need `#[expect(clippy::too_many_lines)]`).

Recommended split:

`gui` (shell):
- `view/` — `workspace_tree`, `detail_view`, `*_view`, formatting helpers
  (`format_pr_status`, `*_label`).
- `command.rs` — the `*_task` builders that wrap `runtime::perform`.
- `selection.rs` — `selected_*` helpers.
- `config.rs` — `AppConfig`, `RawConfig`, `ConfigError`, all `duration_*` /
  `validate_*` helpers.
- `main.rs` — `Message`, `update`, `subscription`, boot.

`gui-core`:
- `state.rs` — `Workspace`, `HostView`, `Message`, `apply`, agent monitor.
- `command.rs` — the `request_host_json` family.
- `connection.rs` — `host_connection_stream`, `StreamState`, `Backoff`,
  `reconcile_interval`, `subscribe_events`, `connect_client`.
- `link.rs` — `SessionLinkMetadata`, `ProviderLaunchItem`, launch flows.
- `ui_state.rs` — `UiState`, `WindowSize`, `Selection`, `TreeNodeId`,
  `default_state_dir`.

Relates to M-SMALLER-CRATES / general module organization. No behavior change;
pure code movement, do it before any feature work on this area.

### H2. Dual source of truth for `selection`

`PohunekApp` keeps the active selection in **two** places that must be hand-kept
in sync:

- `app.ui_state.selection` (persisted, read by every `selected_*` helper in
  `main.rs`, e.g. `main.rs:1251`, `1273`, `1290`, `1689`).
- `app.workspace.selection` (read by `Workspace::selected_github_scope` in
  `lib.rs:1020`, used for GitHub stale-scope rejection).

`update` keeps them aligned only in some arms:
- `SelectSession` (`main.rs:320`) sets `workspace.select_session(...)` **and**
  `ui_state.selection = ...`.
- `SelectProject` (`main.rs:332`) sets workspace then copies
  `ui_state.selection = app.workspace.selection.clone()`.

There are two implementations of "GitHub scope from current selection" —
`main.rs::selected_github_scope`/`selected_project_identity` (over `ui_state`)
and `Workspace::selected_github_scope` (over `workspace.selection`). They can
silently diverge, and the GitHub stale-response guard in `apply` depends on the
workspace copy matching the ui_state copy.

Recommendation: pick one owner. Either make `Workspace` the sole owner of
`selection` and have `UiState` persistence read/write it on load/save, or pass
the selection into `apply` paths explicitly. Remove the duplicate scope helper.

### H3. Inconsistent error routing (UI vs. host state)

Two different error destinations are used for conceptually identical failures:

- Routed to the **global** `app.status` string (transient, single-line):
  `create_session_task`, `inspect_selected_session_task`,
  `stop_selected_session_task`, `metadata_task`, all project CRUD tasks — they
  return `Message::CoreCommandCompleted(Err(String))`, and the `Err` arm sets
  `app.status` (`main.rs:505-508`).
- Routed to **per-host** `last_error` via `CoreMessage::HostOperationFailed`:
  `list_project_actions_task`, `resolve_project_prompt_task`,
  `resolve_project_action_task`, the launch tasks (`lib.rs` maps inner errors to
  `HostOperationFailed`).

So whether a daemon error shows up next to the host in the tree or as a global
status line depends only on which command was invoked. Pick one policy
(recommended: host-scoped `HostOperationFailed` for anything addressed to a
specific host, global status only for pre-flight validation errors) and apply it
uniformly.

---

## Medium priority

### M1. Heavy mechanical duplication in the `*_task` builders (`main.rs`)

Roughly 13 functions follow the identical shape:

```rust
let host = selected_host_config(app)?;
let host_id = host.id.clone();
let options = connection_options(app)?;
let params = ...;
Ok(Task::perform(
    runtime::perform(async move {
        some_call_with_options(&host, params, options)
            .await
            .map(|r| CoreMessage::Variant { host_id, .. })
            .map_err(|err| err.to_string())
    }),
    Message::CoreCommandCompleted,
))
```

Extract a helper that takes the host, options, and an
`FnOnce(HostConfig, ConnectionOptions) -> Future<Output = Result<CoreMessage, _>>`
mapper. This removes ~200 lines and makes the error-routing decision (H3) a
single edit instead of 13.

### M2. The `_with_options` / default-options API is doubled (`lib.rs`)

Every SDK call exists twice: `foo(config, params)` and
`foo_with_options(config, params, options)`. Verified usage: the no-options
variants are called almost exclusively from `gui-core/tests/loopback.rs`
(`create_session`, `list_projects`, `show_project`, etc.); in production only
`load_host_snapshot` (via `load_host`, `lib.rs:1743`) and `host_subscription_stream`
(tests) use them. That is ~13 public functions that are effectively
test-convenience wrappers inflating the crate's public surface.

Options:
- Drop the no-arg variants and have tests pass `ConnectionOptions::default()`
  explicitly (most honest), or
- Feature-gate them as test utilities per the resilience guideline
  ("feature-gate test utilities") so they do not ship as public API.

`pohunek` is explicitly experimental with no back-compat constraint, so this is
a free cleanup.

### M3. Hardcoded `cols: 80, rows: 24` in launch paths

Magic terminal dimensions are inlined and duplicated:

- `launch_prompt_action_task` (`main.rs:955-956`).
- `launch_linear_issue_task` (`main.rs:1144-1145`).
- `launch_github_pull_request_task` (`main.rs:1186-1187`).
- Plus the `NewSessionForm` string defaults `"80"`/`"24"` (`main.rs:175-176`).

This conflicts with the project's "zero hardcoded values" rule and
M-DOCUMENTED-MAGIC. Promote to named constants (e.g.
`DEFAULT_LAUNCH_COLS`/`DEFAULT_LAUNCH_ROWS`) with a one-line rationale, ideally
sourced from `AppConfig` so launched provider sessions inherit a configurable
default.

### M4. No logging/tracing anywhere in the GUI

`grep` for `tracing`/`log`/`eprintln`/`println` across both crates returns
nothing. Errors are only ever surfaced as a UI string and then overwritten by
the next one. For a control plane that fans out to multiple daemons over
NetBird/TCP with a reconnecting state machine, this makes field diagnosis very
hard (e.g. why a host flapped, which request id was rejected as stale).

Recommendation: add `tracing` with structured events per M-LOG-STRUCTURED at the
seams — connection state transitions in `host_connection_stream`, provider
fetch start/result, stale-response rejections in `apply`. Keep tokens/secrets
out (the redaction primitives already exist). This does not need to change what
the user sees in the UI.

### M5. `sh -c` attach spawning interpolates unescaped values (security)

`ShellAttachSpawner::spawn` (`main.rs:2366-2375`) runs the rendered attach
command through `sh -c`. The command is produced by
`render_attach_command` (`lib.rs:2625`), which textually substitutes `{bin}`,
`{host}`, and `{id}` into the operator's template **without shell escaping**.

- `{host}` comes from NetBird host discovery (`discover_hosts`), `{id}` from the
  daemon's session id. Both are normally well-formed, so practical risk is low.
- But any future path where a session id or discovered host name can contain
  shell metacharacters becomes command injection executed in the user's shell.

Contrast with `spawn_notification` (`main.rs:2382`) which correctly uses
`Command::new(command).arg(...).arg(...)` with no shell — that is the safe
pattern. Recommendation: either shell-escape the substituted values, or
restructure the attach template into an argv form so it can be executed without
`sh -c`. At minimum, validate `host`/`id` against an allowlist charset before
substitution.

---

## Low priority / style

### L1. `unreachable!` in `action_prompt_provider` (`lib.rs:2176`)

`ProviderKind::None => unreachable!(...)`. It is currently guarded (callers
handle `None` first), but it is a latent panic in a crate that otherwise sets
`#![forbid(unsafe_code)]` and routes everything through typed errors. Prefer
returning a `CoreError` variant so a future caller cannot turn a logic change
into a panic. The correctness guideline favors total functions over `unreachable!`.

### L2. `WindowSize` width/height are `u32` but effectively clamped to `u16`

`WindowSize { width: u32, height: u32 }` (`lib.rs:784`) is persisted as `u32`,
but `window_dimension_to_f32` (`main.rs:2391`) and `window_dimension_to_u32`
(`main.rs:2400`) clamp through `u16::MAX`. Meanwhile `left_pane_width` and
`agents_pane_height` are `u16`. Either make the window dims `u16` for
consistency, or document why they are wider than they can ever hold.

### L3. Branch-field contract is duplicated across producer and consumer

`LinearIssue::to_prompt_json` emits both `branchName` and `branch`
(`linear.rs:110-111`); `GitHubPullRequest::to_prompt_json` emits both
`headRefName` and `branch` (`github.rs:121-122`). `branch_from_context`
(`lib.rs:2180`) then probes the same multi-name list. The "which JSON key holds
the branch" knowledge lives in two places and must be kept in sync by hand.
Consider a single shared constant/list or a typed intermediate so the contract
has one home.

### L4. Per-operation reconnection cost

`request_host_json` (`lib.rs:2238`) opens a fresh `Client` for every single
command, and `host_connection_stream` separately holds a subscription
connection plus reconnects on each 30s reconcile tick via a new
`load_host_snapshot_with_options`. Acceptable for a low-frequency control plane,
but worth a comment documenting the intentional "connect-per-request" choice so
it is not mistaken for an oversight (M-DOCUMENTED-MAGIC spirit).

### L5. Exponential backoff has no jitter

`Backoff::advance` (`lib.rs:2473`) doubles up to a 30s cap with no jitter. With
several hosts dropping at once this can synchronize reconnect attempts. Minor
for a desktop app with a handful of hosts; add jitter if host counts grow.

### L6. Workspace lint set is slightly narrower than the guideline recommends

The workspace `[lints.clippy]` (root `Cargo.toml`) omits two lints the
M-STATIC-VERIFICATION sample enables: the `cargo` group and `string_to_string`.
Given these crates are binaries/internal libs, the `cargo` group may be
intentional, but `string_to_string` is cheap consistency. Add it (or add a
`reason` for leaving it out, per M-LINT-OVERRIDE-EXPECT discipline).

### L7. `notification_command` silent default

`RawConfig.notification_command` falls back to `"notify-send"` via
`unwrap_or_else` (`main.rs:2433-2435`). This is a sensible, documented platform
default rather than a fabricated required value, so it does not violate the
"no silent defaults" rule — but since the rest of config fails fast, consider
making the platform default explicit (or at least keep the
`DEFAULT_NOTIFICATION_COMMAND` constant comment that explains the choice).

---

## Things that are already good (keep)

- `apply`/`update` centralization with `#[expect(clippy::too_many_lines, reason = ...)]`
  — the reasons are real; once H1 split lands, several of these `#[expect]`s can
  likely be removed.
- Stale-response handling via monotonic `ProviderRequestId` plus scope checks is
  carefully done and well tested (`workspace_ignores_stale_*`,
  `github_provider_ignores_stale_*`).
- Redaction utilities (`redact_auth_tokens`, `redact_bearer_tokens`) and the
  custom `Debug` impls — exactly the right posture; consider a unit test that
  asserts a fake `ghp_`/bearer token never appears in a rendered error (the
  guideline's M-PUBLIC-DEBUG pattern), if one does not already exist.
- Config validation (`validate_http_endpoint`, `non_empty_config_*`,
  `duration_*` rejecting zero) fails fast with typed `ConfigError` — matches the
  "fail fast, no silent defaults" rule.

## Suggested order of work

1. H1 module split (mechanical, unlocks the rest).
2. M1 + M2 + H3 together (the task-builder helper makes uniform error routing a
   one-line policy; drop/feature-gate the no-options API at the same time).
3. H2 single selection owner.
4. M5 attach escaping (security).
5. M3 launch-dimension constants, M4 tracing.
6. L-items opportunistically.
