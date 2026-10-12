---
name: contribute
description: >-
  Guide public work on the Pohunek repository from issue selection through
  eligibility, current reservation limits, implementation, verification,
  pull-request submission, and handoff. Use when planning or making a
  repository contribution; not when operating Pohunek sessions.
---

# Contribute to Pohunek

Help a human or agent contribute through public issues and pull requests. The
accepted permission and custody rules live in
[`.github/agent-workflow.json`](../../../.github/agent-workflow.json) and the
[github-workflow skill](../github-workflow/SKILL.md); follow those sources when
this guide is incomplete. An outside contributor needs no private Project
access or maintainer credentials. A request to contribute grants no repository,
host, credential, model-budget, review, merge, or release authority.

## Choose and check work

Find public scope in the [open issues](https://github.com/zajca/pohunek/issues)
and the [`readiness:ready` queue](https://github.com/zajca/pohunek/issues?q=is%3Aissue%20is%3Aopen%20label%3Areadiness%3Aready).
Use the issue body and comments as the scope and decision record. With the
GitHub CLI, inspect an issue and its linked work with:

```sh
gh issue view NUMBER --repo zajca/pohunek --comments
gh pr list --repo zajca/pohunek --state open --search "NUMBER in:body"
```

Check that the issue has a concrete outcome, stable acceptance criteria,
focused verification, resolved dependencies, capability/size cues, and a
named available review route. Check comments, active assignments, and linked
pull requests for current conflicts. `readiness:ready` is a maintainer-managed
eligibility signal, not proof that work is unclaimed, a reservation, or a
promise to merge. The public policy and current meanings are in
[`publicContribution`](../../../.github/agent-workflow.json).

If scope or acceptance is unclear, ask in the public issue before investing in
implementation. If the work is not listed, propose it using the
[work-item issue form](../../../.github/ISSUE_TEMPLATE/work-item.yml); follow
the [github-workflow skill](../github-workflow/SKILL.md) for issue handling.

## Understand reservation and custody

The binding rules are in
[Public contributions and authority](../github-workflow/SKILL.md#public-contributions-and-authority)
and `contributionPolicy` in
[`.github/agent-workflow.json`](../../../.github/agent-workflow.json). In
practice:

- Automatic reservation is unavailable until the coordination service of
  #805–#811 is delivered. Until then, only a maintainer records a manual reservation
  in public issue evidence. A request, label, assignee, or unanswered comment
  is not a grant. Do not invent or imply a claim command.
- You may propose work, request a reservation, work in a fork you are
  authorized to use, and submit pull requests, including unsolicited ones.
  Without a recognized reservation, keep the work independent and disclose
  conflicts; do not act in an owner's checkout, use someone else's credentials
  or budget, or claim exclusive custody.
- A recognized holder stays within the granted scope and current generation.
  Submission and review retain custody but are not acceptance; expiry starts
  reconciliation rather than silent reassignment.

## Work and verify

Use a branch in your authorized fork and keep changes within the issue's
criteria. Read [AGENTS.md](../../../AGENTS.md) and the task-specific guides it
routes to. For testing boundaries and repository gates, use the
[verification guide](../../../.agents/instructions/verification.md); for issue/project
operations, use the [github-workflow skill](../github-workflow/SKILL.md).

When a task involves operating Pohunek itself, reuse the shipped
[Pohunek agent skill](../../../docs/knowledge/guides/agent-skill.md) for its
CLI and safety contract. This contribution guide does not replace that
operating guidance.

Run the issue's focused checks and every applicable repository gate. Tie each
result to the exact candidate commit and base. Report checks as `passed`,
`failed`, `skipped` with a reason, or `unverified`; never present unknown or
skipped evidence as green. Include generated artifacts and relevant
compatibility evidence when the issue requires them.

## Submit and hand off

Open a pull request from your authorized fork using the
[public pull-request template](../../../.github/pull_request_template.md),
targeting the repository's default branch. A pull request into this repository
can only use one of its own branches as the base, so a fork cannot target the
branch of a preceding slice. Split larger work into the slices described by the
[stacked PR workflow](../../../.agents/instructions/workflow.md) and keep one
slice open at a time: prepare the next slice on its own branch, and open its
pull request only after the slice below it lands. Use `Refs #NUMBER` for a
non-final slice; use `Closes #NUMBER` only on the final slice when the issue's
full definition of done is met. GitHub web or
`gh pr create --repo zajca/pohunek --base main` are both available paths.
Creating a pull request from a fork does not require write access to this
repository; use only an account and fork you are authorized to use. Until a
contributor has a merged contribution, each workflow run on their pull requests
waits for maintainer approval.

In the PR, provide the candidate SHA, base SHA, issue criteria, exact check
commands and outcomes, unresolved work, and the next permitted action. Request
review through the public route defined in
[Public contributions and authority](../github-workflow/SKILL.md#public-contributions-and-authority):
the maintainer `zajca` triages, routes, and integrates after the required gates
and independent review, and an available independent reviewer examines the
code, contract, and evidence of the exact candidate. If no independent reviewer
is available, the candidate stays unaccepted until routing succeeds. You cannot
supply independent approval for your own work, including through another
session or agent. An automated review counts only under the conditions in
`contributionPolicy.reviewAndIntegration.automatedReview` (a completed review of
the exact head after #700, under the corrected #700/#802 contract); an
interrupted or stale-head run is not approval, and the #817 broker's assessment
is not independent review. Review grants no merge authority; release is
separately authorized.

For a handoff, record the issue/spec revision, current reservation and
identity generation when applicable, exact branch/commit/PR, actual checks,
unresolved effects, and next permitted action in public evidence. An
acknowledgment alone does not transfer custody. Follow the same rules for
unsolicited competing pull requests; a reservation guarantees no merge.
