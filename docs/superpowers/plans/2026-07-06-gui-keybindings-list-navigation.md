# GUI Keybindings And List Navigation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish GitHub issue #8 remaining GUI work: configurable keyboard controls and keyboard list navigation.

**Architecture:** Keep keyboard event capture in the Iced shell, but move shortcut meaning into a data-driven keymap. Reuse existing headless provider selected-item state for provider list navigation; keep inbox list cursor as non-persisted shell UI state derived from `Workspace::inbox_rows`.

**Tech Stack:** Rust 2021, Iced, serde/toml, existing `pohunek-gui` and `pohunek-gui-core` unit tests.

---

## Success Criteria

- Default keybindings emit the same messages as the current hardcoded B4 router.
- `route_key_press` resolves `KeyAction` through an effective `KeyMap`.
- `[keybindings]` in `gui.toml` supports partial overrides and fails fast on unknown actions, bad key strings, and same-context chord conflicts.
- Modal and global contexts can reuse the same chord without conflict.
- Inbox, Linear, GitHub PR, and GitHub issue lists support `ListUp`/`ListDown` navigation with wrapping and activation through `Enter`/`o`.
- `/` focuses the active provider search input through a known `text_input::Id`.
- `docs/knowledge/guides/gui.md` documents defaults and override rules, and `docs/knowledge/log.md` records the change.
- Relevant targeted tests pass, then the full repo gates from `AGENTS.md` are run before completion.

## Task 1: K1 Keymap Refactor

**Files:**
- Modify `crates/gui/src/keyboard.rs`
- Modify `crates/gui/src/main.rs`
- Modify `crates/gui/src/command.rs`

- [ ] **Step 1: Write failing routing tests**

Add unit tests in `crates/gui/src/keyboard.rs` proving:

```rust
let keymap = KeyMap::default();
assert_eq!(
    keymap.action_for(KeyContext::Global, &KeyChord::character("1")),
    Some(KeyAction::TabDetail),
);
assert_eq!(
    keymap.action_for(KeyContext::Modal, &KeyChord::shift_named(Named::Enter)),
    Some(KeyAction::ModalPrimaryWithTerminal),
);
```

Also test global tab routing, modifier no-op, GitHub refresh ordering, modal Escape/Enter, and inbox `Shift+Enter` ordering.

- [ ] **Step 2: Run test to verify red**

Run:

```bash
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo test -p pohunek-gui default_keymap_contains_current_b4_shortcuts -- --nocapture
```

Expected: failure because the keymap types do not exist.

- [ ] **Step 3: Implement keymap types and default bindings**

Add `KeyContext`, `KeyAction`, `KeyChord`, and `KeyMap` to `keyboard.rs`. `KeyMap::default()` must encode current B4 shortcuts and reserve `ListUp`, `ListDown`, and `FocusSearch` for B5.

- [ ] **Step 4: Route by action**

Change `route_key_press` to call `route_key_press_with_keymap(app, &KeyMap::default(), key, modifiers)` until K2 wires the configured map. Preserve all current no-op guards and message ordering.

- [ ] **Step 5: Verify green**

Run:

```bash
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo test -p pohunek-gui keyboard -- --nocapture
```

Expected: K1 tests pass.

## Task 2: B5 List Navigation

**Files:**
- Modify `crates/gui-core/src/state.rs`
- Modify `crates/gui/src/message.rs`
- Modify `crates/gui/src/main.rs`
- Modify `crates/gui/src/command.rs`
- Modify `crates/gui/src/keyboard.rs`
- Modify `crates/gui/src/view/inbox.rs`
- Modify `crates/gui/src/view/provider.rs`

- [ ] **Step 1: Write failing provider selection tests**

Add `provider_keyboard_selection_*` tests in `crates/gui-core/src/state.rs` for Linear issues, GitHub PRs, and GitHub issues. Cover next/previous wrapping, filtered visible rows, and empty-list no-ops.

- [ ] **Step 2: Write failing GUI routing tests**

Add `list_navigation_*` tests in `crates/gui/src/keyboard.rs` proving `ListDown`/`ListUp` route to navigation messages and `Enter`/`o` activate the highlighted inbox/provider row.

- [ ] **Step 3: Run tests to verify red**

Run:

```bash
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo test -p pohunek-gui-core provider_keyboard_selection -- --nocapture
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo test -p pohunek-gui list_navigation -- --nocapture
```

Expected: failures because navigation helpers and messages do not exist.

- [ ] **Step 4: Implement provider selection helpers**

Use existing `selected_issue_id`, `selected_pull_request`, and `selected_issue` state. Keep search filtering semantics aligned with `view/provider.rs`.

- [ ] **Step 5: Implement inbox cursor and activation**

Add non-persisted inbox cursor state to `PohunekApp`, render selected rows in `view/inbox.rs`, make `Enter` open the selected message, and make `o` open the selected message's linked session when live.

- [ ] **Step 6: Implement `/` provider search focus**

Add stable `text_input::Id` values for Linear and GitHub search boxes and route `FocusSearch` to the active provider search input.

- [ ] **Step 7: Verify green**

Run the two commands from Step 3 again. Expected: new B5 tests pass.

## Task 3: K2 Config Overrides And Documentation

**Files:**
- Modify `crates/gui/src/config.rs`
- Modify `crates/gui/src/keyboard.rs`
- Modify `crates/gui/src/main.rs`
- Modify `crates/gui/src/command.rs`
- Modify `docs/knowledge/guides/gui.md`
- Modify `docs/knowledge/log.md`

- [ ] **Step 1: Write failing config tests**

Add `keybindings_*` tests covering partial overrides, unknown action names, bad key strings, same-context conflicts, and modal/global chord reuse.

- [ ] **Step 2: Run tests to verify red**

Run:

```bash
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo test -p pohunek-gui keybindings -- --nocapture
```

Expected: failures because raw config conversion does not exist.

- [ ] **Step 3: Implement raw config deserialization and validation**

Add an optional top-level `[keybindings]` table to `RawConfig`, parse action names into `KeyAction`, parse key strings into `KeyChord`, merge with `KeyMap::default()`, and reject same-context duplicate chords bound to different actions.

- [ ] **Step 4: Wire effective keymap into `PohunekApp`**

Add `keymap: KeyMap` to `PohunekApp`. Use `config.keymap.clone()` when config loads, otherwise `KeyMap::default()` so the UI can surface config errors.

- [ ] **Step 5: Document behavior**

Update `docs/knowledge/guides/gui.md` and add a `0.18.3` entry to `docs/knowledge/log.md`.

- [ ] **Step 6: Verify green**

Run:

```bash
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo test -p pohunek-gui keybindings -- --nocapture
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo xtask docs check
```

Expected: config tests and docs check pass.

## Task 4: K3 Effective Keymap Cheat Sheet

**Files:**
- Modify `crates/gui/src/message.rs`
- Modify `crates/gui/src/keyboard.rs`
- Modify `crates/gui/src/view/mod.rs`
- Modify `crates/gui/src/view/modals.rs`
- Modify `docs/knowledge/guides/gui.md`

- [ ] **Step 1: Write failing routing test**

Add a `keymap_help_*` test proving `?` maps to a help action and opens a read-only effective keymap modal.

- [ ] **Step 2: Implement the modal**

Render the effective keymap grouped by context. The modal uses existing Escape/Close behavior.

- [ ] **Step 3: Verify green**

Run:

```bash
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo test -p pohunek-gui keymap_help -- --nocapture
```

Expected: cheat-sheet routing and modal tests pass. If this optional task competes with the required issue scope, the documented override table remains the completion path described by issue #8.

## Final Verification

Run from `/tmp/zremoteng-issue-8`:

```bash
cargo fmt --all --check
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo clippy --workspace --all-targets --all-features
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo test --workspace --all-features
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo build --workspace --release
CARGO_TARGET_DIR=/home/zajca/Code/me/zremoteng/target cargo xtask docs check
```

Then audit GitHub issue #8 requirements against the current diff before reporting completion.
