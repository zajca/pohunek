## Unreleased (2026-11-28, snapshot model id + format_version, #2538 P3a)

- Every `TerminalSnapshot` that crosses a wire now carries `model` /
  `format_version` (issue #2538 P3a), on all three layers:
  `pohunek_terminal::TerminalSnapshot` (`model: ScreenModelId`,
  `format_version: u32`, both `#[serde(default)]`, decoding old JSON as
  `vt100`/1), `worker_protocol::TerminalSnapshot` (`model`/`format_version`
  as `Option`, `skip_serializing_if` when absent), and the public
  `protocol::SessionScreenResult` (`screen_model`/`format_version` as
  `Option` with `ts(optional)`). `ScreenModelId` is `vt100` or `libghostty`
  (snake_case on the wire) and `CURRENT_FORMAT_VERSION = 1` in
  `crates/terminal/src/snapshot.rs`; the worker wire stays at version 9
  (window 8..=9) and the public protocol at 5..=5 — the fields are additive
  in both directions (no `deny_unknown_fields` on any receiving derive in
  either window), so no version bump. `wire_terminal`
  (`crates/session-worker/src/server.rs`) copies both fields into the wire
  snapshot, and the daemon (`crates/daemon/src/session/observation.rs`)
  copies them into `session.screen`, omitting them when the worker did not
  send them, so an N-1 worker produces exactly today's reply shape.
- The case table (plan `a-31d1b862e65e` §4) is pinned: the 12 behaviour rows
  in `crates/terminal/tests/screen_backend.rs`, `tests/links.rs` and
  `crates/session-worker/tests/output_snapshot.rs` each assert the two new
  fields, and the 5 wire rows (new/old worker x new/old daemon, restart,
  vt100 reattach) live in `output_snapshot.rs`.
- Golden fixtures are keyed by `(model, format_version)`:
  `crates/terminal/tests/fixtures/<trace>.<model>.v<N>.{json,ansi}`, with one
  ignored `regenerate_golden_*` test per model (run with
  `cargo test --test screen_backend -- --ignored regenerate_golden`, review
  the diff, commit; #2596 carries the regen doc).
- The default model is NOT flipped in this slice: `crates/terminal/Cargo.toml`
  keeps `default = []`, so `TerminalTracker::new` still runs vt100 and the
  case-table row expecting `new` to report libghostty stays
  `#[ignore = "flipped in P3b ..."]` until the P3b flip.
- Source map: `crates/terminal/src/snapshot.rs`,
  `crates/worker-protocol/src/data.rs` + `lib.rs`,
  `crates/protocol/src/session.rs` + `lib.rs`,
  `crates/session-worker/src/server.rs`,
  `crates/daemon/src/session/observation.rs`,
  `crates/terminal/tests/{screen_backend.rs,links.rs}` + `tests/fixtures/`,
  `crates/session-worker/tests/output_snapshot.rs`, `sdk/ts` regenerated,
  `docs/public-api.md`, `docs/architecture.md`, `changelog.d/`.
