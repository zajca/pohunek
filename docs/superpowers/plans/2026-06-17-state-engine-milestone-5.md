# State Engine Milestone 5 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish a debounced per-session agent activity signal (`working`, `blocked`, `idle`) from PTY output, with a recorded source visible in `session.inspect` and the subscription event stream.

**Architecture:** Keep lifecycle state and agent activity separate. The protocol crate defines `AgentActivity` and the `agent_state` event; the daemon owns a per-session detector task that consumes PTY output through the existing broadcast seam; the detector is split into testable OSC, screen, manifest, and debounce/state-machine modules; the CLI only renders the new `SessionInfo.activity` field.

**Tech Stack:** Rust 2021, Tokio, portable-pty, vt100, toml, regex, serde, serde_json.

---

## Constraints From Former Milestone Notes

- Use `AgentActivity { Working, Blocked, Idle }`; do not overload lifecycle `SessionState`.
- Activity is optional on `SessionInfo` until first detection.
- Publish `agent_state` events with `{ session_id, activity, source }`.
- Use `vt100` for the Phase 1 screen emulator.
- Keep state in memory only.
- Do not implement milestone 6+ agent adapters, real agent manifests, resume, worktrees, SQLite, or event-log files.
- Constants for debounce behavior live in detector config, not as magic numbers.
- Lagged PTY broadcast reads must be logged and resynced.

## File Map

- Modify `crates/protocol/src/session.rs`: add `AgentActivity` and `SessionInfo.activity`.
- Modify `crates/protocol/src/lib.rs`: export `AgentActivity` and add `event::AGENT_STATE`.
- Modify `crates/protocol/src/envelope.rs`: align docs with `activity` naming.
- Modify `crates/protocol/tests/roundtrip.rs`: add protocol JSON round trips.
- Replace `crates/daemon/src/detect/mod.rs`: public detector API and submodule wiring.
- Create `crates/daemon/src/detect/osc.rs`: incremental OSC parser.
- Create `crates/daemon/src/detect/screen.rs`: vt100-backed visible screen extraction and region slicing.
- Create `crates/daemon/src/detect/manifest.rs`: TOML manifest parser and matcher.
- Create `crates/daemon/src/detect/machine.rs`: debounced activity state machine.
- Modify `crates/daemon/src/session/mod.rs`: spawn/cancel detector and publish transitions.
- Modify `crates/daemon/tests/health_socket.rs`: end-to-end activity event and attach coexistence coverage.
- Modify `crates/cli/src/commands/session.rs`: render `activity` in list/inspect.
- Modify root `Cargo.toml` and `crates/daemon/Cargo.toml`: add `vt100`, `toml`, and `regex`.

## Task 1: Protocol Contract

**Files:**
- Modify `crates/protocol/src/session.rs`
- Modify `crates/protocol/src/lib.rs`
- Modify `crates/protocol/src/envelope.rs`
- Test `crates/protocol/tests/roundtrip.rs`

- [ ] Add failing tests for `AgentActivity` serialization: `working`, `blocked`, `idle`.
- [ ] Add a failing `SessionInfo` JSON test where `activity: Some(AgentActivity::Working)` serializes as `"activity": "working"`.
- [ ] Add or update a failing `SessionInfo` JSON test proving `activity: None` is omitted.
- [ ] Add a failing `agent_state` event test using `event::AGENT_STATE` and flattened payload keys `session_id`, `activity`, and `source`.
- [ ] Run `rtk cargo test -p zagentmesh-protocol --test roundtrip`; expected failure is missing type/field/constant.
- [ ] Implement `AgentActivity` with snake_case serde names.
- [ ] Add `SessionInfo.activity` with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
- [ ] Export `AgentActivity` and `event::AGENT_STATE`.
- [ ] Update the envelope doc example from `"state"` to `"activity"` for `agent_state`.
- [ ] Run `rtk cargo test -p zagentmesh-protocol --test roundtrip`; expected pass.

## Task 2: Detector Core Modules

**Files:**
- Replace `crates/daemon/src/detect/mod.rs`
- Create `crates/daemon/src/detect/osc.rs`
- Create `crates/daemon/src/detect/screen.rs`
- Create `crates/daemon/src/detect/manifest.rs`
- Create `crates/daemon/src/detect/machine.rs`
- Modify root `Cargo.toml`
- Modify `crates/daemon/Cargo.toml`

- [ ] Add daemon dependencies `vt100`, `toml`, and `regex`.
- [ ] Add failing OSC parser tests for fragmented OSC 0/2 titles, OSC 9 progress, BEL termination, ST termination, and aborted/stray sequences.
- [ ] Implement an incremental OSC parser that buffers across chunks and emits title/progress evidence.
- [ ] Add failing screen tests for visible output, bottom-line region extraction, and CJK/wide glyph slicing without off-by-one column errors.
- [ ] Implement a vt100-backed `ScreenTracker` with region slicing APIs needed by manifests.
- [ ] Add failing manifest tests for priority resolution, `contains`, `regex`, `line_regex`, nested `all`/`any`/`not`, case-insensitive contains, and complexity caps.
- [ ] Implement manifest parsing and matching with caps: 128 rules, 512 gates, 1024 matchers, depth 8.
- [ ] Add failing debounce tests for bytes-flowing working detection, startup grace, repeated confirmation of idle/blocked, cap behavior, and flicker collapse.
- [ ] Implement `DetectionConfig` constants: recheck 100 ms, confirmations 3, cap 700 ms, stable-visible refresh 800 ms, startup grace 3 s.
- [ ] Implement a detector API that consumes byte chunks and returns confirmed `ActivityTransition { activity, source }` values.
- [ ] Run targeted detector unit tests with `rtk cargo test -p zagentmesh-daemon detect`; expected pass.

## Task 3: Session Registry Integration

**Files:**
- Modify `crates/daemon/src/session/mod.rs`
- Test `crates/daemon/src/session/mod.rs`

- [ ] Add a failing unit test proving a detector-published transition updates `SessionInfo.activity`, `state_source`, and `updated_at`.
- [ ] Add a failing unit or integration test proving session stop/exit cancels detector ownership cleanly.
- [ ] Extend `SessionEntry` with detector cancellation/ownership state.
- [ ] Spawn the detector task in `SessionRegistry::create()` after the session is inserted and before returning created info.
- [ ] On each confirmed transition, update the session snapshot and emit `event::AGENT_STATE` with flattened `{ session_id, activity, source }`.
- [ ] On PTY output `Lagged`, log skipped chunks and resync detector state instead of silently continuing.
- [ ] Cancel detector ownership in `stop()`/`record_exit()` alongside attach cleanup.
- [ ] Run targeted session tests with `rtk cargo test -p zagentmesh-daemon session`; expected pass.

## Task 4: CLI Surface

**Files:**
- Modify `crates/cli/src/commands/session.rs`

- [ ] Add failing render tests for `session inspect` showing `activity` as `<none>` before detection and `working` after detection.
- [ ] Add a failing render test for `session list` including an `ACTIVITY` column.
- [ ] Implement `activity_label()` and render activity in human list/inspect output.
- [ ] Leave JSON output unchanged except for the typed protocol field naturally serialized by `SessionInfo`.
- [ ] Run `rtk cargo test -p zagentmesh-cli`; expected pass.

## Task 5: End-to-End Daemon Tests

**Files:**
- Modify `crates/daemon/tests/health_socket.rs`

- [ ] Add an end-to-end OSC test: create shell session, subscribe, attach, write `printf '\033]0;working\007'`, assert `agent_state` with `activity = working` and `source = osc_title`, then inspect and assert the same state.
- [ ] Add detector + attach coexistence coverage by keeping a raw attach stream open while asserting the detection event arrives and raw output remains readable.
- [ ] Add no-leaked-detector coverage through stop/clean shutdown behavior available in the current test harness.
- [ ] Run `rtk cargo test -p zagentmesh-daemon --test health_socket`; expected pass outside the sandbox when Unix socket bind is restricted.

## Task 6: Final Verification

**Files:**
- Whole workspace

- [ ] Run `rtk cargo fmt --all`.
- [ ] Run `rtk cargo build`.
- [ ] Run `rtk cargo clippy --all-targets --workspace`.
- [ ] Run `rtk cargo test --workspace`.
- [ ] Inspect `rtk git diff --stat` and `rtk git diff` for accidental milestone 6+ scope.
- [ ] Run a final code review subagent over the full diff.
