# Nested Agent Reporting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Detect and surface an agent launched inside an existing shell session, without changing the shell session's launch/resume identity.

**Architecture:** Keep `SessionInfo.agent` and `agent_base` as immutable launch identity. Add optional active-agent fields, reported through new session-level hook callback methods and used to reconfigure the session detector manifest at runtime. Agent hooks report active agent identity by inherited `POHUNEK_*` environment; native resume metadata for nested agents is exposed as active-agent metadata only and never persisted as the parent session resume binding.

**Tech Stack:** Rust 2021, Tokio, Serde JSON protocol, `vt100`-backed detector pipeline, shell hook assets, existing `cargo test`/`cargo clippy` gates.

**Do not commit:** Repository instructions say commits are only made on explicit user request.

---

## File Structure

- `crates/protocol/src/session.rs`: add optional active-agent fields to `SessionInfo`; add report/release params and result structs; extend agent filter matching.
- `crates/protocol/src/envelope.rs`: add `StateSource::Report`.
- `crates/protocol/src/lib.rs`: export new protocol types and method constants.
- `crates/protocol/tests/roundtrip.rs`: pin wire shapes, method names, optional-field omission, and `StateSource::Report`.
- `crates/daemon/src/detect/mod.rs`: make `DetectorConfig` cloneable and add detector reconfiguration.
- `crates/daemon/src/session/target.rs`: store a detector config sender and default detector config in each session entry.
- `crates/daemon/src/session/detector.rs`: listen for detector-config changes.
- `crates/daemon/src/session/mod.rs`: add active-agent internal state, `report_agent`, and `release_agent`; inject hook env into shell sessions; preserve `report_native_id` restrictions.
- `crates/daemon/src/api/handler.rs`: dispatch `session.report_agent` and `session.release_agent`.
- `crates/daemon/src/integration/assets/{codex,claude}/pohunek-agent-state.sh`: report active-agent identity on `SessionStart`.
- `crates/daemon/src/integration/mod.rs`: install hook assets in a way that keeps existing SessionStart behavior and supports shell-session inheritance.
- `crates/cli/src/commands/session.rs`: render active-agent information in list/inspect and mirror agent filtering.
- `crates/cli/src/commands/attach.rs`: show active agent in the attach banner when present.
- `crates/gui-core/src/lib.rs`: parse `StateSource::Report` through existing typed `SessionInfo` and `AgentStateEvent` paths; adjust tests/fixtures for new fields when needed.
- `docs/public-api.md`, `docs/architecture.md`, `docs/knowledge/concepts/sessions.md`, `docs/knowledge/assistant/source-map.md`: document the new callback methods, active-agent fields, events, and nested-agent invariant.

## Success Criteria

- A `session.new --agent shell` session receives enough `POHUNEK_*` env for Codex/Claude hooks launched inside it to call back.
- A nested Codex/Claude `SessionStart` report sets `active_agent`, `active_agent_base`, and active native session metadata on the shell session.
- `SessionInfo.agent` remains `shell`; nested reports never populate or overwrite `native_session_id` / `native_session_path`.
- The detector switches from the shell manifest to the active agent manifest while the nested agent is active, then back on release.
- `session.list --filter agent=codex` matches a shell session with active Codex, while JSON still shows launch `agent: "shell"`.
- CLI list/inspect/attach surfaces the active agent clearly.
- Protocol changes are additive: new optional fields, new methods, and a new enum value.
- Targeted tests demonstrate red/green coverage before implementation; full repo gates are attempted before final status.

---

### Task 1: Protocol Surface and Roundtrip Tests

**Files:**
- Modify: `crates/protocol/src/session.rs`
- Modify: `crates/protocol/src/envelope.rs`
- Modify: `crates/protocol/src/lib.rs`
- Test: `crates/protocol/tests/roundtrip.rs`

- [ ] **Step 1: Write failing protocol tests**

Add tests for:

```rust
#[test]
fn session_report_agent_method_names_are_stable() {
    assert_eq!(method::SESSION_REPORT_AGENT, "session.report_agent");
    assert_eq!(method::SESSION_RELEASE_AGENT, "session.release_agent");
}

#[test]
fn session_report_agent_params_roundtrip_with_optional_native_refs() {
    let params = SessionReportAgentParams {
        session_id: SessionId("s-42".to_owned()),
        source: "pohunek:codex".to_owned(),
        agent: "codex".to_owned(),
        activity: Some(AgentActivity::Working),
        seq: Some(123),
        agent_session_id: Some("codex-native".to_owned()),
        agent_session_path: None,
    };
    let value = serde_json::to_value(&params).expect("serialize report params");
    assert_eq!(value["session_id"], "s-42");
    assert_eq!(value["source"], "pohunek:codex");
    assert_eq!(value["agent"], "codex");
    assert_eq!(value["activity"], "working");
    assert_eq!(value["seq"], 123);
    assert_eq!(value["agent_session_id"], "codex-native");
    assert!(value.get("agent_session_path").is_none());
    assert_eq!(line_roundtrip(&params), params);
}

#[test]
fn session_info_roundtrips_with_active_agent_fields() {
    let mut info = running_shell_session();
    info.active_agent = Some("codex".to_owned());
    info.active_agent_base = Some(AgentKind::Codex);
    info.active_agent_session_id = Some("codex-native".to_owned());
    info.active_agent_session_path = None;
    let value = serde_json::to_value(&info).expect("serialize session info");
    assert_eq!(value["agent"], "shell");
    assert_eq!(value["active_agent"], "codex");
    assert_eq!(value["active_agent_base"], "codex");
    assert_eq!(line_roundtrip(&info), info);
}
```

Also add a `StateSource::Report` serialization assertion.

- [ ] **Step 2: Run protocol tests and verify failure**

Run: `rtk cargo test -p pohunek-protocol roundtrip --all-features`

Expected: compilation fails because `SessionReportAgentParams`, `SessionReleaseAgentParams`, result types, methods, fields, and `StateSource::Report` do not exist.

- [ ] **Step 3: Implement protocol types**

Add optional fields to `SessionInfo`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub active_agent: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub active_agent_base: Option<AgentKind>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub active_agent_session_id: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub active_agent_session_path: Option<String>,
```

Add report/release structs:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReportAgentParams {
    pub session_id: SessionId,
    pub source: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReleaseAgentParams {
    pub session_id: SessionId,
    pub source: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReportAgentResult {
    pub recorded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReleaseAgentResult {
    pub released: bool,
}
```

Extend `SessionListFilter::Agent` to match launch identity or active identity:

```rust
Self::Agent(name) => {
    session.agent == *name
        || base_kind_label(session.agent_base) == name
        || session.active_agent.as_deref() == Some(name)
        || session
            .active_agent_base
            .is_some_and(|base| base_kind_label(base) == name)
}
```

Add `StateSource::Report` with docs and export new types/method constants from `lib.rs`.

- [ ] **Step 4: Run protocol tests and verify pass**

Run: `rtk cargo test -p pohunek-protocol roundtrip --all-features`

Expected: pass.

---

### Task 2: Daemon Active-Agent State and Report/Release API

**Files:**
- Modify: `crates/daemon/src/session/mod.rs`
- Modify: `crates/daemon/src/session/target.rs`
- Modify: `crates/daemon/src/api/handler.rs`
- Test: `crates/daemon/src/session/mod.rs`
- Test: `crates/daemon/tests/health_socket.rs`

- [ ] **Step 1: Write failing daemon unit tests**

Add tests that create a shell session, call `report_agent`, and assert:

```rust
assert_eq!(inspected.agent, "shell");
assert_eq!(inspected.agent_base, AgentKind::Shell);
assert_eq!(inspected.active_agent.as_deref(), Some("codex"));
assert_eq!(inspected.active_agent_base, Some(AgentKind::Codex));
assert_eq!(inspected.active_agent_session_id.as_deref(), Some("codex-native"));
assert_eq!(inspected.native_session_id, None);
assert_eq!(inspected.activity, Some(AgentActivity::Working));
assert_eq!(inspected.state_source, StateSource::Report);
```

Add release tests that clear active-agent fields only when `source`, `agent`, and `seq` are current; stale lower `seq` releases must be ignored.

Add a socket/API test that sends:

```json
{"method":"session.report_agent","params":{"session_id":"s-1","source":"pohunek:codex","agent":"codex","activity":"blocked","seq":1}}
```

Expected response: `{"recorded":true}` and an `agent_state` event with `source:"report"`.

- [ ] **Step 2: Run daemon tests and verify failure**

Run: `rtk cargo test -p pohunek-daemon report_agent --all-features`

Expected: compilation fails because methods and fields do not exist.

- [ ] **Step 3: Implement internal state**

Add an internal active report record:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveAgentReport {
    source: String,
    agent: String,
    seq: Option<u64>,
}
```

Extend `SessionEntry` with:

```rust
active_agent: Option<ActiveAgentReport>,
detector_config: watch::Sender<DetectorConfig>,
default_detector_config: DetectorConfig,
```

Add helper functions:

```rust
fn report_is_current(current: Option<&ActiveAgentReport>, source: &str, agent: &str, seq: Option<u64>) -> bool
fn release_matches(current: &ActiveAgentReport, source: &str, agent: &str, seq: Option<u64>) -> bool
```

Use `seq` as monotonic protection only within the same source+agent pair. If either side has no sequence, accept the new report/release.

- [ ] **Step 4: Implement `SessionRegistry::report_agent`**

Resolve `params.agent` with `ProfileRegistry::resolve_agent`. Unknown or invalid agents return `recorded:false`, not a protocol error, because hooks are fire-and-forget.

On a valid live session:

- update active-agent fields;
- validate optional `agent_session_id` via `SessionRef::id`;
- validate optional `agent_session_path` via `SessionRef::path`;
- send a new detector config for the active agent;
- if `activity` is present, set `activity` and `state_source = StateSource::Report`;
- emit `session_updated`;
- emit `agent_state` when activity was present.

- [ ] **Step 5: Implement `SessionRegistry::release_agent`**

When release matches the current active report:

- clear active-agent fields;
- clear active native fields;
- clear `activity`;
- reset `state_source = StateSource::Process`;
- restore the default detector config;
- emit `session_updated`.

Unknown, terminal, stale, or mismatched releases return `released:false`.

- [ ] **Step 6: Add API dispatch**

Parse the new params and return `ok` results from `handle_request`.

- [ ] **Step 7: Run daemon report tests**

Run: `rtk cargo test -p pohunek-daemon report_agent --all-features`

Expected: pass.

---

### Task 3: Runtime Detector Reconfiguration

**Files:**
- Modify: `crates/daemon/src/detect/mod.rs`
- Modify: `crates/daemon/src/session/detector.rs`
- Modify: `crates/daemon/src/session/target.rs`
- Test: `crates/daemon/src/detect/mod.rs`
- Test: `crates/daemon/tests/health_socket.rs`

- [ ] **Step 1: Write failing detector tests**

Add a detector unit test:

```rust
#[test]
fn detector_reconfigure_switches_manifest_without_losing_screen() {
    let started_at = instant();
    let mut detector = Detector::new(8, 80, started_at, DetectorConfig::generic_shell());
    assert!(detector.feed(started_at, b"\x1b]2;\xe2\xa0\x80 thinking\x07").is_empty());
    detector.reconfigure(
        started_at + Duration::from_millis(10),
        DetectorConfig::codex(),
    );
    assert_eq!(
        detector.tick(started_at + Duration::from_secs(4)),
        vec![transition(AgentActivity::Working, StateSource::OscTitle)]
    );
}
```

Add an integration test where a shell session is reported as active Codex, then a Codex OSC title emits `working` through the Codex manifest.

- [ ] **Step 2: Run detector tests and verify failure**

Run: `rtk cargo test -p pohunek-daemon detector_reconfigure --all-features`

Expected: compilation fails because `Detector::reconfigure` and config watch plumbing do not exist.

- [ ] **Step 3: Implement detector config cloning and reconfigure**

Derive/implement `Clone` for `DetectorConfig`.

Add:

```rust
pub fn reconfigure(&mut self, now: Instant, config: DetectorConfig) {
    self.state = StateMachine::new(now, config.detection);
    self.manifest = config.manifest;
    self.process_activity.reset();
    self.state.clear_pending();
}
```

Keep latest OSC title/progress and screen content so an already visible agent UI can be re-evaluated immediately.

- [ ] **Step 4: Wire detector watch channel**

Create a `watch::channel(default_detector_config.clone())` in `register_pty_session`, store the sender in `SessionEntry`, and pass the receiver into `spawn_detector`.

In `spawn_detector`, add a `changed = detector_config_rx.changed()` branch that calls `detector.reconfigure(Instant::now(), detector_config_rx.borrow().clone())`.

- [ ] **Step 5: Run detector tests**

Run: `rtk cargo test -p pohunek-daemon detector --all-features`

Expected: pass.

---

### Task 4: Hook Environment and Hook Assets

**Files:**
- Modify: `crates/daemon/src/session/hooks.rs`
- Modify: `crates/daemon/src/integration/assets/codex/pohunek-agent-state.sh`
- Modify: `crates/daemon/src/integration/assets/claude/pohunek-agent-state.sh`
- Modify: `crates/daemon/src/integration/mod.rs`
- Test: `crates/daemon/src/session/mod.rs`
- Test: `crates/daemon/src/integration/mod.rs`

- [ ] **Step 1: Write failing env and asset tests**

Update `session_pty_env_marks_session_id_for_every_agent_kind` or add a new test asserting shell sessions include:

```rust
assert_eq!(lookup(ENV_FLAG).as_deref(), Some("1"));
assert_eq!(lookup(ENV_SOCKET_PATH).as_deref(), Some("/tmp/pohunek.sock"));
assert_eq!(lookup(ENV_SESSION_ID).as_deref(), Some("s-7"));
assert_eq!(lookup(ENV_PROTOCOL_VERSION).as_deref(), Some("1"));
```

Add asset tests that assert Codex/Claude hook assets contain `session.report_agent` and still contain `session.report_native_id`.

- [ ] **Step 2: Run tests and verify failure**

Run: `rtk cargo test -p pohunek-daemon session_pty_env session_report_agent assets --all-features`

Expected: failure because shell lacks hook env and assets only report native id.

- [ ] **Step 3: Inject hook env into shell sessions**

Change `hook_env` so every agent kind gets hook env when `socket_path` exists. Keep `session_pty_env` deduping `POHUNEK_SESSION_ID`.

Preserve `POHUNEK_DAEMON_ID` for every session.

- [ ] **Step 4: Update hook assets**

For `SessionStart` action:

- read native id/transcript path as today;
- send `session.report_agent` first with `source = "pohunek:<agent>"`, `agent`, and optional `agent_session_id` / `agent_session_path`;
- send `session.report_native_id` second as today, so sessions launched directly as Codex/Claude remain resumable;
- ignore failures in both calls.

The hook script must remain POSIX shell + Python 3 and keep the 0.5s socket timeout.

- [ ] **Step 5: Run env/asset tests**

Run: `rtk cargo test -p pohunek-daemon session_pty_env assets --all-features`

Expected: pass.

---

### Task 5: CLI and GUI-Core Surface

**Files:**
- Modify: `crates/cli/src/commands/session.rs`
- Modify: `crates/cli/src/commands/attach.rs`
- Modify: `crates/gui-core/src/lib.rs`
- Test: `crates/cli/src/commands/session.rs`
- Test: `crates/cli/src/commands/attach.rs`
- Test: `crates/gui-core/src/lib.rs`

- [ ] **Step 1: Write failing CLI tests**

Add tests asserting:

- list table renders a shell session with active Codex as `shell->codex`;
- inspect includes `active_agent`, `active_agent_base`, and active native session fields;
- local filter `agent=codex` matches a shell session whose active agent is Codex;
- attach banner updates agent display from `session_updated.active_agent`.

- [ ] **Step 2: Run CLI tests and verify failure**

Run: `rtk cargo test -p pohunek-cli session attach --all-features`

Expected: failure because rendering ignores active-agent fields.

- [ ] **Step 3: Implement CLI labels**

Add:

```rust
fn agent_display(info: &SessionInfo) -> String {
    match info.active_agent.as_deref() {
        Some(active) if active != info.agent => format!("{}/{}", info.agent, active),
        _ => agent_label(&info.agent).to_owned(),
    }
}
```

Use it for list and inspect. Update attach snapshot to prefer `active_agent` when present and different, preserving the launch agent in the display.

- [ ] **Step 4: Update gui-core tests if needed**

`SessionInfo` deserialization should work through protocol types. Add a narrow test only if existing fixtures need explicit active-agent expectations.

- [ ] **Step 5: Run CLI/gui-core tests**

Run:

```bash
rtk cargo test -p pohunek-cli session attach --all-features
rtk cargo test -p pohunek-gui-core --all-features
```

Expected: pass.

---

### Task 6: Documentation and Knowledge Bundle

**Files:**
- Modify: `docs/public-api.md`
- Modify: `docs/architecture.md`
- Modify: `docs/knowledge/concepts/sessions.md`
- Modify: `docs/knowledge/assistant/source-map.md`

- [ ] **Step 1: Update public API docs**

Document:

- new `SessionInfo.active_agent*` fields;
- `session.report_agent`;
- `session.release_agent`;
- `StateSource::Report`;
- `agent_state` source may be `report`.

- [ ] **Step 2: Update concept docs**

Document that nested agent reports are active-runtime metadata only. They do not alter launch `agent`, `agent_base`, or resume binding.

- [ ] **Step 3: Update architecture**

Replace the old absolute statement that hooks only capture native IDs with the refined model:

- native resume hooks still capture launch-agent resume metadata;
- nested-agent hook callbacks may report active runtime identity;
- live state remains detector-first, with report source as explicit hook evidence.

- [ ] **Step 4: Run docs check**

Run: `rtk cargo xtask docs check`

Expected: pass.

---

### Task 7: Final Verification

**Files:**
- All touched files.

- [ ] **Step 1: Run focused tests**

Run:

```bash
rtk cargo test -p pohunek-protocol --all-features
rtk cargo test -p pohunek-daemon report_agent --all-features
rtk cargo test -p pohunek-cli session attach --all-features
rtk cargo test -p pohunek-gui-core --all-features
```

Expected: pass.

- [ ] **Step 2: Run full gates**

Run:

```bash
rtk cargo fmt --all --check
rtk cargo clippy --workspace --all-targets --all-features
rtk cargo test --workspace --all-features
rtk cargo build --workspace --release
rtk cargo xtask docs check
```

Expected: pass. If any gate fails for an unrelated pre-existing reason, record the exact command and failure.

- [ ] **Step 3: Review invariants**

Check:

- `report_native_id` still rejects different launch/base agents.
- Active-agent native refs are not persisted in resume binding store.
- Unknown/invalid hook reports return `recorded:false`.
- Release cannot clear a newer report with an older sequence.
- JSON without active-agent fields still deserializes.
