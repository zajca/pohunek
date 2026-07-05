# GUI code + UX refactor plan (2026-07-05)

Combines the architecture-review GUI refactors (G1 message split, G2 module
decomposition) with a UX redesign. UX spec authored by a UX pass (Fable 5),
grounded in `docs/design/track-d-ui-brief.md` and the current
`crates/gui/src/main.rs` / `crates/gui-core/src/lib.rs`.

Confirmed direction (operator preferences + review + brief):

- **Navigation:** right pane gets a persistent tab bar
  `Detail · Linear · GitHub · Worktrees`; Inbox and message detail become a
  modal; Linear/GitHub item detail stay enriched modals.
- **Order:** code refactor (G2 module split, then G1 message split) first, then
  UX phases on the clean structure.
- **Scope of first pass:** refactor + UX phases 1–3 (inbox modal, tab bar, rich
  item detail incl. Linear GraphQL extension). Keyboard layer (phases 4–5) is a
  follow-up pass.

Nothing here changes the daemon wire protocol. The Linear GraphQL extension is
inside the gui-core provider surface only.

## Phased engineering order

### Track A — code refactor (behavior-preserving, gate-verified)

- **A1. gui-core module split** — carve `crates/gui-core/src/lib.rs` (~4.7k) into
  `state.rs` (`Workspace`/`HostView`/`apply`/agent monitor), `sdk.rs`
  (`request_*`/`*_with_options`), `connection.rs`
  (`host_connection_stream`/`Backoff`/`reconcile_interval`), `link.rs`
  (session-link metadata + launch flows), `ui_state.rs`
  (`UiState`/`Selection`/`TreeNodeId`/`WindowSize`), `message.rs` (the enum).
  Pure code motion.
- **A2. gui shell module split** — carve `crates/gui/src/main.rs` (~5.7k) into
  `message.rs`, `update.rs`, `command.rs` (`*_task` builders +
  `push_provider_task_result`), `selection.rs`, `config.rs`, `attach.rs`, and
  `view/{mod,tree,detail,inbox,provider,session,project,modals,toast}.rs`.
  `main.rs` keeps boot + subscription + theme.
- **A3. Message split (G1)** — split `gui-core::Message` into `DomainEvent`
  (async I/O results the core reduces) vs UI intents (constructed by the shell,
  mutating provider/UI state through typed methods rather than the wire enum).

### Track B — UX (per the spec below)

- **B1 (phase 1). Inbox modal** — `ModalView::Inbox` with two layers
  (`InboxView::List | Message`); move inbox/notification content into it;
  auto-mark-read on open; primary **Open session** button; collapse filters to
  `Needs action | All | Archived` + host pick_list; delete `Selection::Notification`
  routing and the five filter-row fns.
- **B2 (phase 2). Tab bar** — `ActiveTab` in app state + `UiState`; split
  `project_pane` so worktrees and provider browser become tab bodies;
  force-Detail tab on `SelectSession`; disabled-tab styling when no project scope.
- **B3 (phase 3). Rich item modals + list rows** — recompose Linear/PR modals
  (state pill, assignee, branch + Copy + Open-in-browser via `xdg-open` argv,
  review/checks pills); upgrade list rows; add `Message::OpenUrl`. Extend the
  Linear GraphQL query for `state`/`assignee`/`updatedAt` (render conditionally).

### Follow-up pass (not this scope)

- **B4/B5. Keyboard layer** — `on_key_press` subscription: `1-4` tabs, `i` inbox,
  `b` blocked-agent cycle, `o` open terminal, `n` new, `a` assistant, `r` refresh,
  `Esc`/`Enter` in modals; then `j/k` list nav + `/` search focus. Has an Iced
  focus-guard caveat (no cheap "is an input focused" query).

---

## UX spec (verbatim, Fable 5)

### A. Navigation model — persistent tab bar in the detail area, messages as modal

Decision: the right pane gets a persistent 4-tab bar
`Detail · Linear · GitHub · Worktrees`, and the Inbox moves entirely into a modal.
The current model (`detail_view`, main.rs:2807) swaps the whole pane by selection
type, and `project_pane` (main.rs:3484) stacks project detail + worktrees +
provider browser into one tall scroll — that is why navigation feels incoherent:
Linear/GitHub/worktrees are only reachable *through* a project selection, and
selecting a notification hijacks the pane.

- Tab strip always visible at the top of the right pane; style like the existing
  `ProviderPanel` toggle (main.rs:4156-4164) but full-width, with `1 2 3 4` hints
  in labels.
- Context chip at the right end shows scope `host / project-label` (derived as
  `provider_browser_view` does via `selected_host_id` / `selected_github_scope`,
  main.rs:4137, 4165). Session selection → scope = that session's project.
  Missing scope → tabs 2-4 render "select a project" empty state.
- Tab contents: **Detail** = current selection-driven pane (session/project/host/
  start), with `project_pane` slimmed to identity + rename + New session + actions.
  **Linear** = `linear_provider_view` promoted to full tab. **GitHub** =
  `github_provider_view` promoted. **Worktrees** = `project_worktrees` promoted
  (drop its `.height(360)` cap).
- Selection → tab rule: selecting a session anywhere force-switches to Detail
  (triage must never land behind a Linear tab). Selecting project/host does not
  switch tabs.
- Persist `active_tab` in `UiState`.
- `Selection::Notification` is deleted as a pane route; notifications become a modal.

Modal vs tab: Linear/GitHub browse = **tab** (long-lived scannable lists, persist
state). Worktrees = **tab**. Linear/PR item detail = **modal** (transient
pick→preview→launch, already exists at main.rs:4037). Inbox list+detail = **modal**
(interrupt queue, operator's ask). Prompts = not a tab; stay in Start/Launch modals
(`start_modal_content`, main.rs:3858).

### B. Inbox / messages redesign

Entry: left-rail Inbox button (main.rs:2422) opens `ModalView::Inbox` (keeps unread
count); per-host `inbox N` tree buttons (main.rs:2523-2530) open it pre-filtered to
that host; keyboard `i`.

Modal — two layers, one `ModalView`. Wider `dialog_card` (~760px, ~80% max-height,
internal scroll). New state `inbox_view: InboxView::List | InboxView::Message(host_id, notification_id)`.
`Esc`/Back walks Message → List → closed.

Layer 1 — list. Header `Inbox · N unread` + filter control + Close. Replace the five
chip rows (`notification_filters`, main.rs:2887-2897) with two controls:
segmented `Needs action | All | Archived` (`Needs action` = unread OR severity in
{ActionRequired, Error}; default), and a Host pick_list (only when 2+ hosts have
notifications). Severity/kind/provider filtering disappears as UI; data stays.
Sort: `AgentBlocked`/`ApprovalRequired` pinned top with an `action` pill, then
unread by recency, then read. Row (reuse `notification_row`, main.rs:3074):
severity dot · title (bold when unread) · right-aligned age; second muted line
`host · session-name-or-id · kind`. Row click → Layer 2; `j/k`/arrows move, `Enter`
opens, `o` jumps straight to session.

Layer 2 — message detail. Header (Back, title, severity pill), meta line, scrollable
plain-text body, buttons `[Open session] [Acknowledge] [Archive] [Delete]  > Details`.
`Open session` is primary (exists as `notification_link_action`, main.rs:3281; keep
liveness guard + dead-session fallback): close modal → `SelectSession` → Detail tab.
`Enter` = Open session; `Shift+Enter` also fires `Message::OpenSession` (terminal).
Auto-mark-read on open (delete `Mark read`, main.rs:3202-3210). `> Details` expander
replaces `notification_summary` (main.rs:3146) + `notification_metadata` (main.rs:3257):
source triplet, created, project id, metadata k/v, dedupe/source ids.

Blocked-first: agents monitor stays primary; an `AgentBlocked` notification's
Open-session lands on the same session. (Ties into the backlog resolve-on-resume
for stale attention notifications; `Needs action` default makes stale unread more
visible.)

### C. Provider item detail

Both stay modals; both get a real layout instead of three stacked lines
(main.rs:4051-4062).

Linear issue modal: header `IDENT [state pill]` + Close; title (wraps); meta
`assignee · updated`; `branch [Copy] [Open in browser]`; scrollable plain-text body
(~360 high, no markdown — Iced brief §8); session-name input; `Action [pick_list]
[Launch]` (Enter). Fields today (linear.rs:82-95): identifier, title, body, branch,
url. `state`, `assignee`, `updatedAt` are NOT fetched — needs a GraphQL extension in
`gui-core/src/providers/linear.rs`; render pill/meta only when present so UI ships
first. `Open in browser` = new `Message::OpenUrl(String)` spawning `xdg-open` via
argv (`Command::new`), never `sh -c` (avoids an M5-style injection surface). Copy
branch reuses the `CopyWorktreePath` clipboard task. Monospace only for
identifier/branch. List rows in the Linear tab also upgrade: `IDENT` (mono) · title ·
muted branch second line.

GitHub PR modal: header `#num title [draft][state]` + Close; `author · labels`;
`branch [Copy] [Open in browser]`; `[review pill] [checks: N pass / N fail / N pending]`;
scrollable body; session-name + Action + Launch. Everything fetched already
(github.rs:102-127); pills exist (`review_pill` main.rs:4498, `status_pill` 4475,
check summary 4436) — pure composition. GitHub issue modal: same treatment, keep
"reference-only; launch from a pull request" (main.rs:4090), add Open in browser.

### D. Keyboard shortcuts

No keyboard handling today (`subscription`, main.rs:2306-2319, is only window-resize
+ host streams). Net-new via `keyboard::on_key_press`.

Global (no modal, no text input focused): `1 2 3 4` switch tab; `i` inbox; `b` select
first Blocked agent + Detail (repeat cycles); `o` open selected session in terminal;
`n` new session; `a` assistant; `r` refresh active tab; `/` focus active provider
search; `j/k`/arrows move list selection (phase 5). Killer path `b → o`.

Modal-scoped: `Esc` back/close; `Enter` primary (Launch / Open session);
`Shift+Enter` in inbox detail = Open session + terminal; `j/k`/arrows + `Enter`/`o`
in inbox list.

Iced caveat: `on_key_press` fires globally and Iced has no cheap "is any input
focused" query. Mitigation: track focus optimistically (set on `/`-focus and any
`on_input`, clear on Esc/submit); prefer digits/`Esc`/`Enter` first, single-letter
keys behind the focus guard.

### E. States (per surface)

- Tab strip: always rendered; tabs 2-4 disabled-styled when no project scope
  (tooltip "Select a project"); context chip shows scope host conn dot; a down scope
  host renders the tab body's error state, tabs stay clickable.
- Linear tab: empty "No issues — Fetch to load" / token unconfigured message (never
  token entry); loading "Fetching issues..." row, list stays; error = `last_error`
  danger card, stale list kept with hint; per-host scope isolates failure.
- GitHub tab: scope mismatch keeps "Fetch GitHub data for the selected project"
  (main.rs:4331); `gh` missing → explicit error card; loading/error as Linear.
- Worktrees tab: empty "No worktrees for this project." (main.rs:3702); loading
  "Worktree details not loaded yet — Refresh" (main.rs:3695); daemon error via host
  `last_error` + inline card.
- Inbox modal: `Needs action` empty "All clear."; `All` empty "No notifications";
  down host's rows persist with its conn state in the meta line.
- Item modals: keep "No issue selected" guards (main.rs:4049, 4095); error card
  inside modal with Launch disabled.

### F. Phased implementation order (UX)

1. Inbox modal (highest operator pain, mostly rewiring). Also removes one
   `Selection` variant (helps gui-review H2).
2. Tab bar (new `ActiveTab` persisted; split `project_pane`; force-Detail on
   `SelectSession`).
3. Rich item modals + list rows (composition + `Message::OpenUrl`; Linear GraphQL
   extension, rendered conditionally).
4. Keyboard layer phase 1 (digits, `i b o n a r`, `Esc`/`Enter`, focus guard).
5. Keyboard layer phase 2 (`j/k` list nav, `/` search focus).

Prereq: gui-review H1 (splitting view fns out of the 5.7k main.rs) should land
before/with phase 2 — the tab refactor touches every `*_pane`.
