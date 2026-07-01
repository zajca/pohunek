# GUI Assistant Launch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `pohunek-gui` start a Universal Pohunek Assistant session from a modal that chooses intent, agent profile, and request.

**Architecture:** Move the assistant launch orchestration into `pohunek-gui-core` as a shared SDK-style API, then route the CLI through that API. The Iced shell remains a thin view/update layer that collects modal state and dispatches a `Task` to `gui-core`.

**Tech Stack:** Rust 2021, Tokio, Iced 0.14, serde/serde_json, thiserror, existing `pohunek-client`, `pohunek-knowledge`, `pohunek-protocol`, and `pohunek-gui-core`.

---

## File Map

- Modify `crates/gui-core/Cargo.toml`: depend on `pohunek-knowledge` with the `protocol` feature.
- Create `crates/gui-core/src/assistant.rs`: shared assistant launch types and orchestration.
- Modify `crates/gui-core/src/lib.rs`: expose assistant API and use it from tests.
- Modify `crates/gui-core/tests/loopback.rs`: cover shared launch behavior against the loopback daemon fixture.
- Modify `crates/cli/Cargo.toml`: remove direct knowledge dependency only if it becomes unused after the move.
- Modify `crates/cli/src/commands/assistant/mod.rs`: delegate launch orchestration to `gui-core`.
- Keep or shrink `crates/cli/src/commands/assistant/{prompt,select,snapshot,bootstrap}.rs` based on what moved.
- Modify `crates/cli/tests/assistant.rs`: keep parser expectations green.
- Modify `crates/gui/src/main.rs`: add assistant modal state, messages, picker UI, launch task, and workspace button.
- Modify `docs/knowledge/guides/gui.md`: document the GUI assistant entry point.
- Modify `docs/knowledge/assistant/source-map.md`: add the shared assistant implementation file.

## Task 1: Shared Assistant Types and Selection

**Files:**
- Create `crates/gui-core/src/assistant.rs`
- Modify `crates/gui-core/src/lib.rs`

- [ ] Write failing unit tests for `select_agent`:

```rust
#[test]
fn auto_agent_prefers_pohunek_assistant_then_codex() {
    let caps = caps(vec![("codex", true), ("pohunek-assistant", true)]);
    let selected = assistant::select_agent(&caps, None).expect("agent selected");
    assert_eq!(selected.name, "pohunek-assistant");
}
```

- [ ] Write failing unit tests for explicit agent selection:

```rust
#[test]
fn explicit_agent_wins_even_when_not_in_runtime_list() {
    let selected = assistant::select_agent(&caps(vec![("codex", true)]), Some("custom"))
        .expect("explicit agent selected");
    assert_eq!(selected.name, "custom");
}
```

- [ ] Run `rtk cargo test -p pohunek-gui-core assistant::`; expected failure is missing module/API.
- [ ] Implement these public core types:

```rust
pub mod assistant;

pub struct LaunchParams {
    pub intent: Intent,
    pub request: Option<String>,
    pub agent: Option<String>,
    pub host: String,
    pub project: Option<String>,
    pub repo: Option<PathBuf>,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub no_snapshot: bool,
    pub degraded: bool,
    pub no_start_daemon: bool,
}
```

- [ ] Implement `Intent`, `AgentSelection`, `select_agent`, and target validation in `assistant.rs`.
- [ ] Run `rtk cargo test -p pohunek-gui-core assistant::`; expected pass.

## Task 2: Shared Launch Orchestration

**Files:**
- Modify `crates/gui-core/src/assistant.rs`
- Modify `crates/gui-core/Cargo.toml`
- Modify `crates/gui-core/tests/loopback.rs`

- [ ] Write a failing loopback test proving launch sends `session.new` with an assistant prompt and selected project:

```rust
let result = assistant::launch_with_options(&host, params, options).await?;
assert_eq!(result.session.project_id.as_deref(), Some("ui"));
assert!(result.applied_input == Some(true));
```

- [ ] Run `rtk cargo test -p pohunek-gui-core --test loopback assistant`; expected failure is missing launch API.
- [ ] Move or recreate the CLI assistant primitives in `gui-core::assistant`: bootstrap, snapshot collection, materialization, prompt compose, read preflight, and launch.
- [ ] Keep degraded remote validation identical: remote degraded launches return an error before `session.new`.
- [ ] Ensure the API returns `SessionNewResult` plus metadata needed by CLI JSON output.
- [ ] Run `rtk cargo test -p pohunek-gui-core --test loopback assistant`; expected pass.

## Task 3: CLI Delegation

**Files:**
- Modify `crates/cli/src/commands/assistant/mod.rs`
- Modify `crates/cli/src/lib.rs`
- Modify `crates/cli/Cargo.toml`
- Modify `crates/cli/tests/assistant.rs`

- [ ] Write or keep failing CLI tests that assert all existing assistant flags parse.
- [ ] Replace CLI-local orchestration with conversion from `AssistantOptions` into `pohunek_gui_core::assistant::LaunchParams`.
- [ ] Preserve CLI-only output behavior: `--json`, `--print-prompt`, human lines, and remote confirmation.
- [ ] Run `rtk cargo test -p pohunek-cli assistant`; expected pass.

## Task 4: GUI Modal State and Launch Task

**Files:**
- Modify `crates/gui/src/main.rs`

- [ ] Add failing tests for selected project context:

```rust
let target = selected_assistant_project(&app).expect("project target");
assert_eq!(target.project_ref, "selected-project");
```

- [ ] Add `AssistantForm` with intent, agent, request editor, advanced flags, branch, and base branch.
- [ ] Add `ModalView::Assistant`, open/close messages, picker messages, and `LaunchAssistant`.
- [ ] Add `assistant_modal_content(app)` with context text, intent picker, agent picker, request editor, Advanced controls, and start button.
- [ ] Add `assistant_launch_task(app)` that calls `pohunek_gui_core::assistant::launch_with_options`.
- [ ] Reuse the existing `CoreMessage::SessionCreated` completion path so successful launches auto-attach.
- [ ] Run `rtk cargo test -p pohunek-gui`; expected pass.

## Task 5: Workspace Entry Point

**Files:**
- Modify `crates/gui/src/main.rs`

- [ ] Add a compact `Assistant` button above the workspace tree:

```rust
button(row![text("◎").size(14), text("Assistant").size(14)])
    .on_press(Message::OpenAssistantModal)
```

- [ ] Disable launch through validation rather than hiding the button when no project is selected; the status error should explain the missing context.
- [ ] Run `rtk cargo test -p pohunek-gui`; expected pass.

## Task 6: Knowledge Docs

**Files:**
- Modify `docs/knowledge/guides/gui.md`
- Modify `docs/knowledge/assistant/source-map.md`

- [ ] Document the GUI assistant button, modal fields, and launch semantics.
- [ ] Add `crates/gui-core/src/assistant.rs` to the source map.
- [ ] Run `rtk cargo xtask docs check`; expected pass.

## Task 7: Full Verification and Merge

**Files:**
- All touched files.

- [ ] Run `rtk cargo fmt --all --check`; expected pass.
- [ ] Run `rtk cargo clippy --workspace --all-targets --all-features`; expected pass.
- [ ] Run `rtk cargo test --workspace --all-features`; expected pass.
- [ ] Run `rtk cargo build --workspace --release`; expected pass.
- [ ] Run `rtk cargo xtask docs check`; expected pass.
- [ ] Commit the current branch with a concise English message.
- [ ] Merge the current branch into `main` after verification.
