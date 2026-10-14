## Unreleased (2026-10-13, identity sequences ordered per process, #879)

- The worker orders identity claims and releases per reported process (pid and
  kernel start identity): a report must be newer than every claim and release
  accepted for that same process, and reports of different processes are never
  compared. A descendant's report of its own process, even at
  `sequence = u64::MAX`, no longer makes the launch root's later reports stale,
  so the root still establishes the launch identity and updates its native
  reference. Across processes the latest accepted claim is the active identity.
- The worker keeps the sequence of every live reporting process and never
  evicts it, so a released process cannot become active again through its own
  older claim delivered late. Marks of exited processes are pruned when room is
  needed; the PTY root and the launch process are always kept. When live
  reporters besides those two fill the bound (16), a further process's report
  is refused with the new reason `identity_reporters_exhausted` until one
  exits; an exited process its parent has not reaped counts as exited. Before
  the launch identity exists, a launch agent's report with a native reference
  is verified as a launch claim even at the bound and kept only when it
  establishes the launch identity, so a full table cannot keep a descendant
  launch process from being pinned; a deferred verification keeps only its
  pending launch claim for the retry. A release answered `ok: false` still
  orders its process's later claims.
- A report of another process accepted as the active identity clears the
  background owner's snapshot (`agent_changed`) whatever its sequence, since
  sequences of different processes are never compared; the clear raises the
  background mark to the worker's own clock, so an owner snapshot taken before
  the report and delivered after it stays stale.
- `docs/public-api.md`, the runtime packages guide and the sessions concept
  state the per-process order; the sequence contract is a value strictly
  increasing across every hook event that reports the same process.
