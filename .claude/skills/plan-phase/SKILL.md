---
name: plan-phase
description: >-
  Plan the next pohunek phase interactively — review the roadmap and current
  state, resolve open questions ONE at a time with the user deciding, then
  record a complete end-to-end plan as a GitHub issue (no PoC, no shortcuts).
  Use when the user asks to plan or design the next phase or milestone before
  implementing it.
---

# plan-phase — plan the next phase into a GitHub issue

Produces the spec that the `milestone` skill later implements: a complete,
end-to-end plan recorded as a GitHub issue, arrived at through an interactive
question-and-decision pass with the user. Planning is where this project
refuses shortcuts — the point is a full design, not a minimal proof of
concept. The plan is persisted in the issue via the `github-workflow` skill;
no local `NEXT.md` is written.

## Steps

1. **Resolve the target issue.** Route through the `github-workflow` skill's
   issue-resolution rules: use the issue URL/number the user or task gives;
   otherwise deduplicate against existing issues first. When the user hands
   over concrete new phase scope and no matching issue exists, **auto-create**
   the planning issue and add it to the configured project without a further
   ask; ask only when the scope is genuinely ambiguous or competing issues
   both plausibly cover it. Create the issue when planning starts (so
   decisions land somewhere live as they are made) and grow the body as
   questions are settled.
2. **Ground yourself in the current state.** Read before proposing:
   `docs/ROADMAP.md`, `docs/phases/`, `docs/architecture.md` (authoritative
   scope), the relevant `docs/design/*.md` (accepted technical constraints
   stay authoritative until a deliberate decision revises them — then the
   issue must say so), and the current open issues that touch the same
   surfaces. Skim the crates the phase will touch so the plan is grounded in
   the real code, not assumptions.
3. **Frame the phase.** State what this phase is for and where it sits in the
   roadmap. List the key assumptions explicitly. Respect the repo's hard
   constraints (owner-first direct operation, owner WebUI, additive optional
   relay, PTY/TUI-first, remote over NetBird not SSH, providers shell-out and
   client-only, no back-compat shims).
4. **Resolve open questions — one at a time.** This is the core of the skill and
   how the user works: for each unresolved question, ask it on its own, describe
   what the problem is, and offer 2-3 concrete options with trade-offs. Wait for
   the user's decision before moving to the next question. Do not batch every
   question at once, and do not silently pick an answer. Record each decision
   and its rationale in the issue body as it is
   settled.
5. **Write the complete plan into the issue.** The issue body (via the
   `github-workflow` skill) covers the phase end to end: scope, the design and
   decisions recorded above, the crates/surfaces affected (protocol ripples
   into `client`/`daemon`/`cli`/`gui-core` if the wire changes),
   knowledge-bundle and `docs/public-api.md` impact, and an explicit, testable
   definition-of-done list with stable IDs (`D1`, `D2`, ...) the `milestone`
   and `milestone-review` skills will check against. Ensure the issue is in
   the configured project for delivery tracking. Being tracked as `Todo` does
   not imply the proposed design is accepted; acceptance is an explicit
   decision recorded on the issue.
6. **Confirm.** Summarize the plan and the DoD, link the issue, and note that
   it is ready to implement (via the `milestone` skill).

## Hard rules

- **No PoC, no minimal versions, no shortcuts** unless the user explicitly asks
  for a reduced scope. The plan must describe the full solution.
- Best solution over fastest — never trade correctness or completeness for
  implementation speed during planning.
- **The plan does not authorize implementation or merge.** It defines what was
  agreed; building it still goes through the `milestone` skill, and landing it
  through `merge-advance`/`pr-handoff`.
- Planning goes here as a GitHub issue. Design docs that outlive a milestone
  belong under `docs/design/` only when the user asks to store them there; do
  not create local `NEXT.md` or RFC files as work authorities.
