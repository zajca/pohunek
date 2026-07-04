# Codex Prompt: Remaining Architecture Review Work

Use this prompt in a fresh Codex session after the
`zajca/architecture-review-2026-07-04-fixups` branch has been merged into
`main`.

```text
Work in /home/zajca/Code/me/zremoteng.

Goal: continue the remaining architecture-review work documented in
docs/architecture-review-2026-07-04-remaining.md. Use a new git worktree from
main for the implementation branch. Do not work directly on main.

Required repository instructions:
- Read AGENTS.md before making changes.
- Check for CLAUDE.md and task-relevant .claude/ guidance if present.
- Before editing Rust files, read the applicable .agents/rust-guidelines files,
  starting with .agents/rust-guidelines/SKILL.md and
  .agents/rust-guidelines/11_universal_guidelines.md.
- Keep all repository files, code, comments, documentation, branch names, commit
  messages, and PR text in English.
- Keep changes scoped and update docs/knowledge when behavior or public API
  changes require it.

Use subagents:
- Use a controller/subagent workflow. The controller owns planning, integration,
  final verification, and conflict resolution.
- Dispatch fresh implementation subagents for independent tasks only after the
  controller has read the relevant plan and code context.
- Give each implementation subagent a narrow file/module ownership scope and the
  full task text it needs. Do not make subagents infer scope from vague pointers.
- For broad daemon, GUI, or protocol design work, let subagents inherit the main
  model unless the task is clearly mechanical and isolated.
- Prefer gpt-5.3-codex for bounded coding subagents when the controller has a
  clear plan.
- Prefer gpt-5.3-codex-spark only for fast read-only exploration, mechanical
  cleanup, or small test-fix loops.
- Do not run multiple implementation subagents in parallel when they may edit
  overlapping files.
- After each implementation subagent, run a spec-compliance review subagent and
  then a code-quality review subagent. Fix and re-review any findings before
  moving to the next task.
- Use read-only exploration subagents in parallel when mapping independent code
  areas, such as daemon session internals, GUI message flow, and SDK callsites.

Suggested first implementation slice:
1. Read docs/architecture-review-2026-07-04-remaining.md.
2. Create a short execution plan for one bounded slice, not the whole backlog.
3. Start with either:
   - GUI message split and related gui-core tests, or
   - SDK typed-call migration for remaining normal request/response callsites.
4. Avoid starting the daemon SessionRegistryInner decomposition until there is a
   precise owner-boundary plan and enough test coverage around lifecycle,
   attach, subscriptions, and resume binding.

Acceptance criteria for any completed slice:
- Existing behavior is preserved unless the plan explicitly says otherwise.
- New business logic, state transitions, protocol behavior, and utilities have
  tests proportional to risk.
- Any public CLI, protocol, GUI, or knowledge-bundle behavior changes update the
  matching documentation in the same branch.
- No mock implementation, placeholder, or TODO is introduced for required
  behavior.
- No secrets or environment-specific configuration are hardcoded.

Verification before completion:
- Run the relevant narrow test loop while developing.
- Before claiming the branch is complete, run:
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features
  cargo test --workspace --all-features
  cargo build --workspace --release
  cargo xtask docs check
- If restricted sandboxing blocks socket-binding daemon/transport tests, rerun
  the affected command outside the sandbox instead of weakening the tests.

Expected final response:
- State which remaining-review slice was implemented.
- List the main files changed.
- Report exact verification commands and results.
- Call out any remaining review items that were intentionally left for a later
  branch.
```
