---
name: plan-phase
description: >-
  Plan the next pohunek phase interactively — review the roadmap and current
  state, resolve open questions ONE at a time with the user deciding, then write
  a complete end-to-end NEXT.md (no PoC, no shortcuts). Use when the user says
  "napiš plán na novou fázi", "naplánuj NEXT.md", "udělej NEXT.md pro další
  fázi", "poďme naplánovat další fázi", "plan the next phase", "co je dál v
  plánu", or asks to design the next milestone before implementing it.
---

# plan-phase — plan the next phase into NEXT.md

Produces the spec that the `milestone` skill later implements: a complete,
end-to-end `NEXT.md` for the next phase, arrived at through an interactive
question-and-decision pass with the user. Planning is where this project refuses
shortcuts — the point is a full design, not a minimal proof of concept.

## Steps

1. **Ground yourself in the current state.** Read the direction and status docs
   before proposing anything: `docs/ROADMAP.md`, `docs/phases/`,
   `docs/architecture.md` (authoritative scope), any relevant `docs/design/*.md`,
   and the current `NEXT.md` if one exists. Skim the crates the phase will touch
   so the plan is grounded in the real code, not assumptions.

2. **Frame the phase.** State what this phase is for and where it sits in the
   roadmap. List the key assumptions explicitly. Respect the repo's hard
   constraints (single operator, no central server, PTY/TUI-first, remote over
   NetBird not SSH, providers shell-out and client-only, no back-compat shims).

3. **Resolve open questions — one at a time.** This is the core of the skill and
   how the user works: for each unresolved question, ask it on its own, describe
   what the problem is, and offer 2-3 concrete options with trade-offs. Wait for
   the user's decision before moving to the next question. Do not batch every
   question at once, and do not silently pick an answer. ("Dej mi postupně na
   každou nevyřešenou otázku dotaz a popiš v čem je problém a jaké jsou návrhy
   řešení.")

4. **Write the complete NEXT.md.** Once the questions are settled, write
   `NEXT.md` at the repo root covering the phase end to end: scope, the design
   decided above, the crates/surfaces affected (protocol ripples into
   `client`/`daemon`/`cli`/`gui-core` if the wire changes), knowledge-bundle and
   `docs/public-api.md` impact, and an explicit, testable definition-of-done list
   the `milestone` and `milestone-review` skills will check against.

5. **Confirm.** Summarize the plan and the DoD, and note that `NEXT.md` is ready
   to implement (via the `milestone` skill).

## Hard rules

- **No PoC, no minimal versions, no shortcuts** unless the user explicitly asks
  for a reduced scope. The plan must describe the full solution. ("Ano zapiš
  plný rozsah, žádné zkratky, žádné kompromisy.")
- Best solution over fastest — never trade correctness or completeness for
  implementation speed during planning.
- `NEXT.md` is transient and uncommitted; it is consumed and replaced each
  milestone (see the `merge-advance` skill). Design docs that outlive a
  milestone belong under `docs/design/`.
