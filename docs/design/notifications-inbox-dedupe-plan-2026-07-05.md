# Notifications inbox dedupe/debounce plan (2026-07-05)

Status: proposed. Diagnosis complete; implementation not started.

The inbox fills up with dozens of notifications from a single session. Live
data from the local daemon (2026-07-05): 68 records total, 59 of them from
session `s-46` — 35 permanently-unread `turn_completed` records with **no
dedupe key**, 24 acknowledged attention records sharing the single key
`attention:s-46`, in a repeating `turn_completed` → (+~65 s) → `agent_blocked`
pattern for every Claude turn.

> **Scope:** daemon notification store/coordinator/projector, the claude and
> codex hook assets, knowledge bundle. No GUI changes required; the inbox is
> rendering exactly what the daemon persists.

## N0. Diagnosis (done)

### The pipeline today

1. **Hook assets** (`crates/daemon/src/integration/assets/{claude,codex}/pohunek-agent-notify.sh`)
   map provider events to `notification.create`:
   - `Notification.idle_prompt` / `permission_prompt` / `elicitation_dialog` →
     attention kinds (`agent_blocked`, `approval_required`) **with**
     `dedupe_key = attention:<session_id>` (claude asset, lines 63-127 and
     270-273).
   - `Stop` → `turn_completed`, `attention: False` → **no dedupe key at all**.
   - `source_id` embeds a per-event millisecond timestamp
     (`hook:claude:Stop:1783237433737`), so it is unique per event by design.
2. **Create path** (`crates/daemon/src/notifications/mod.rs`,
   `store.rs:135-200 create_or_dedupe`):
   - Idempotency dedupe matches same source namespace + `source_id` — only
     collapses redelivery of the *same* hook event, never two turns.
   - Key-based dedupe runs only when `dedupe_key` is present, and only inside
     `attention_dedupe_window_secs` (120 s, `store.rs:159`).
   - The debounce coordinator (`coordinator.rs`) defers only attention creates:
     `is_attention_create` = attention kind **and** `attention:` dedupe key
     (`mod.rs:876-881`). Everything else persists immediately.
3. **Resolution** (`projector.rs:294-332`): the edge into
   `AgentActivity::Working` calls `resolve_session_attention`, which
   acknowledges visible records for `attention:<sid>` — and the store's
   `resolve_attention` explicitly **skips non-attention kinds**.
4. **GUI inbox** (`gui-core/src/state.rs:259-280`): default scope
   `NeedsAction` shows everything unread. Correctly.

### Root causes, in order of impact

- **RC1 — `turn_completed` is exempt from every dedupe/debounce/resolve
  mechanism.** No dedupe key (skips key dedupe and the coordinator), unique
  `source_id` (skips idempotency dedupe), non-attention kind (skipped by
  resolve-on-resume). Every Claude/Codex `Stop` therefore appends one record
  that stays **unread forever** unless manually acted on. 35 of the 59 s-46
  records are exactly this.
- **RC2 — double record per idle moment.** Claude fires `Stop` when the turn
  ends and `Notification.idle_prompt` ~60 s later. The attention record is
  debounced, deduped, and auto-acknowledged on resume (works as designed); its
  `turn_completed` twin from the same moment stays unread. The s-46 timeline
  shows the `T` / `T+65 s` pairs for every turn.
- **RC3 — policy enables it per provider.** The built-in default policy
  disables `turn_completed`, but the live policy has `claude.turn_completed:
  true` and `codex.turn_completed: true` overrides (operator-set via
  `pohunek notifications policy`). Enabling a kind must not mean "unbounded
  unread growth"; the fix keeps the kind useful instead of telling the
  operator to turn it off.
- **RC4 (minor, by design) — attention history accumulates.** Each
  block→resume cycle acknowledges the visible attention record and the next
  block creates a fresh one; the 120 s dedupe window intentionally does not
  span cycles. 24 acknowledged records are correct behavior, visible only in
  the `All` scope. No change planned; retention pruning already covers it.

### Verification recipe

Reproduce: `banner=false` attach to a Claude session, let a turn finish, wait
70 s, repeat twice. Observe two new unread `turn_completed` plus acknowledged
`agent_blocked` records via `pohunek notifications list --json`. After the fix
the same sequence must leave **at most one** visible row for the session.

## N1. Session-scoped dedupe key + supersede semantics for `turn_completed`

**Hook assets (both providers):** when a session id is present, `stop` events
send `dedupe_key = turn:<session_id>` (new prefix constant, sibling of
`attention:<session_id>`). Bump `POHUNEK_INTEGRATION_VERSION` in both managed
scripts so `pohunek integration install` refreshes them; extend the mapping
tables in the `integration/mod.rs` tests (`~1784`, `~1814`) with the dedupe
column for `stop`.

**Store (`create_or_dedupe`):** add a supersede rule for `turn:` keys — when a
candidate carries a `turn:` dedupe key and an **unread** record with the same
key exists (no time window; the previous turn's completion is stale the moment
a newer one exists):

1. transition the existing record to `Acknowledged` with
   `superseded_by = candidate.id` (the field already exists on
   `NotificationRecord` and is barely used — this gives it its real purpose),
   emitting `notification_updated`;
2. append the candidate as `Created`.

Invariant: **at most one unread `turn_completed` per session**, older ones
remain in `All` scope history with a supersede link.

Rejected alternative — update-in-place (refresh one record's `created_at` and
reset it to unread): fewer records, but it rewrites history in an append-only
log, reshuffles the inbox row identity the GUI deliberately keeps stable, and
loses the per-turn trail. Supersede keeps ids immutable.

**Constants:** `turn_dedupe_key(session_id)` lives beside
`attention_dedupe_key` in `daemon/src/notifications` (exported to the
projector). The string prefix is duplicated into the two shell assets, pinned
by the integration mapping tests, same as the `attention:` prefix today.

## N2. Consume-on-activity and cross-kind collapse

**Working edge acks turns too.** Extend the projector's `Working`-edge resolve
(`projector.rs:311-313`) to resolve both keys: `attention:<sid>` (unchanged)
and `turn:<sid>`. Generalize the store's `resolve_attention` into a
key-scoped resolve whose kind predicate follows the key prefix (`attention:` →
attention kinds, `turn:` → `turn_completed`) instead of hard-coding attention
kinds; the coordinator's `Resolve` command already carries the key string, so
the coordinator itself only needs the projector to send both keys.

Effect: the moment the operator answers the agent (session goes `Working`),
the now-consumed "turn completed" row disappears from `NeedsAction` — the
same resolve-on-resume contract attention records already have.

**Attention commit supersedes the turn twin (RC2).** When an attention record
for session S becomes visible (both the immediate-create path and the
coordinator flush path go through `NotificationService::commit_record`),
acknowledge any unread `turn:<S>` record with
`superseded_by = <attention record id>`. Rationale: `agent_blocked` strictly
subsumes "turn completed" — the agent finished *and* is waiting. The
`T`/`T+65 s` pair then renders as a single visible row that escalates from
info to warning instead of two rows.

## N3. Debounce `turn_completed` through the coordinator

Route `turn:`-keyed creates through the existing `AttentionCoordinator` defer
path instead of persisting immediately:

- Generalize `is_attention_create` (`mod.rs:876-881`) into a
  "debounced create" predicate covering both `attention:`-keyed attention
  kinds and `turn:`-keyed `turn_completed`. The coordinator machinery
  (pending map, `DelayQueue`, generation guard) is already key-generic; only
  the routing predicate and the projector's resolve calls change.
- Reuse `attention_debounce_secs` (5 s) as the window. A new policy field
  would be configuration surface without a demonstrated need; revisit only if
  turn noise at 5 s proves different from attention noise. Document the shared
  window in the policy field docs.
- Behavior: if the operator's reply lands within the window (session returns
  to `Working`), the pending `turn_completed` is dropped and **never
  persisted** — an attached operator who answers immediately generates no
  record at all.

Priority interplay: the coordinator's cross-producer priority
(`outranks`/`upgrade_projector`) is keyed per dedupe key and `turn:` keys only
ever come from provider hooks, so no priority changes are needed.

## N4. Tests and docs (part of each change, not a follow-up)

- **Store:** supersede on second unread turn (no window), superseded record
  keeps history in `All`, key-scoped resolve acks `turn:` records and still
  skips unrelated kinds, attention commit supersedes the turn twin.
- **Coordinator:** turn create defers; resolve-within-window drops it
  unpersisted; flush after window persists exactly one record.
- **Projector:** `Working` edge resolves both keys; blocked edge still creates
  attention only.
- **Integration:** hook mapping tables updated for the `stop` dedupe key and
  the bumped asset version.
- **Knowledge bundle + public API** (same change, per repo rule): document the
  notification lifecycle — dedupe keys (`attention:`/`turn:`), debounce,
  resolve-on-resume, supersede — in `docs/knowledge/guides/gui.md` (inbox
  section) and `docs/public-api.md` (`notification.create` dedupe-key
  semantics). Run `cargo xtask docs check`.
- Full gate set: `cargo fmt --all --check`, `cargo clippy --workspace
  --all-targets --all-features` (`-D warnings`), `cargo test --workspace
  --all-features`, `cargo build --workspace --release`.

## N5. Operator cleanup (no code)

The fix is forward-looking; the existing 68-record backlog stays. One-time
cleanup after deploying:

- `pohunek notifications prune --status unread --before <now>` (or ack-all
  from the GUI inbox) to clear the accumulated `turn_completed` records;
- `pohunek integration install` on every host so the updated hook assets with
  the `turn:` dedupe key take effect;
- optionally `pohunek notifications policy` to disable `turn_completed` per
  provider if the operator decides the kind is not worth keeping even
  deduplicated.

## Order and dependencies

1. **N1** — hook dedupe key + store supersede (bounds the growth immediately;
   independently shippable).
2. **N2** — working-edge resolve + attention-commit collapse (removes the
   remaining per-turn residue; depends on N1's key).
3. **N3** — coordinator debounce for turns (suppresses records entirely for
   attended sessions; depends on N1's key, benefits from N2's resolve wiring).
4. **N4** runs inside each step (tests + bundle in the same change); **N5** is
   an operator action after release.

Open implementer decisions: none blocking. The only judgment call is the
superseded record's terminal status (`Acknowledged` chosen here over
`Archived` so the history stays in `All` without extra scope changes) — keep
it unless a test reveals a scope interaction.
