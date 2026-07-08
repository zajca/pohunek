# Terminal attach plan (2026-07-05): mouse offset fix + session control modal

Two tracks for the `pohunek attach` composited terminal (banner mode):

- **Track M** — diagnose and fix the one-row mouse offset that appears when the
  attach banner is enabled and the agent (Claude Code) uses the mouse.
- **Track C** — an in-terminal session control modal: a keyboard shortcut opens
  an overlay above the live session with session actions (kill, detach, new
  session in the same worktree, rename, fork).

Both tracks live in the same two files' orbit: `crates/terminal` (compositor)
and `crates/cli/src/commands/attach.rs` (attach loop). Track M lands first —
it touches the exact stdin-forwarding path the modal must later intercept, and
a correct mouse coordinate space is a prerequisite for any future mouse support
in the modal.

> **Decisions (operator, 2026-07-05):** the menu trigger **repurposes
> `Ctrl-\`** (instant kill moves inside the menu behind a confirm), and
> **C3 (fork) is in scope** for this track. Track C3 is the only part that
> touches the wire protocol; everything else is client-side.

---

## Track M — mouse offset under the banner

### M0. Diagnosis (root cause, confirmed from code)

The compositor reserves physical row 1 for the banner and renders the agent
grid on physical rows `2..=N`:

- `BANNER_ROWS = 1` — `crates/terminal/src/compositor.rs:37`
- scroll region `2..=rows` established in `write_setup` — `compositor.rs:191`
- the agent PTY is sized `(rows - 1, cols)` — `compositor.rs:122`,
  `effective_attach_size` in `crates/cli/src/commands/attach.rs:549`

The agent's input modes — including mouse reporting (DECSET 1000/1002/1003 and
encoding 1005/1006) — are deliberately propagated from the parsed grid to the
*physical* terminal via `write_input_modes` (`compositor.rs:226`, doc comment
`compositor.rs:23-27`). The physical terminal therefore reports mouse events in
**physical** coordinates: a click on physical row `R` produces a report with
row `R`.

The attach loop forwards stdin to the daemon PTY **verbatim**
(`attach.rs:897`), so the agent receives row `R` while the clicked cell is its
grid row `R - 1`. Every mouse event is off by exactly `BANNER_ROWS` (one row) —
precisely the reported symptom, and only when the banner is on.

The existing test `attach_banner_non_kill_input_is_forwarded`
(`attach.rs:1526-1530`) even pins the buggy behavior: it feeds the SGR mouse
report `\x1b[<64;7;1M` and asserts it is not consumed — i.e. forwarded
untranslated. That test changes meaning in M1.

Column coordinates are unaffected (the grid starts at physical column 1).

**Verification recipe (before and after the fix):**

1. `launcher.conf`: `banner=true`; attach to a Claude Code session.
2. Click a target with a known row (e.g. a menu entry or a text position in the
   composer). Before the fix the click lands one row above the intended cell
   inside Claude's UI; after the fix it lands exactly.
3. Wheel-scroll and drag-select as regression checks (wheel and motion reports
   carry the same coordinate encoding).
4. Toggle `banner=false` and confirm passthrough behavior is untouched.

**Related audit (same frame, cheap):** coordinate-carrying *replies* the
terminal sends on stdin. The main one is the cursor-position report
(CPR, `ESC [ row ; col R`, answer to `ESC [ 6 n`). Under the compositor the
agent's DSR query is parsed into the grid and never reaches the physical
terminal, so today the agent gets **no** CPR reply at all (a separate latent
issue, out of scope); but if the operator's terminal emits an unsolicited CPR
it would also be off by one. M1's translator should handle CPR rows the same
way. Focus events (DECSET 1004, `ESC [ I` / `ESC [ O`) carry no coordinates —
plain passthrough.

### M1. Fix — a stateful input translator in `crates/terminal`

New module `crates/terminal/src/input.rs` (name deliberately generic: it
translates *terminal → agent* input reports, mouse today, CPR alongside):

- **`InputTranslator`** — a small stateful byte-stream processor:
  `push(&[u8]) -> Vec<u8>` (or an internal buffer + `take_output()`), owning a
  partial-sequence buffer so reports split across `read()` boundaries (8 KiB
  stdin buffer, `attach.rs:30`) are reassembled before translation. Non-report
  bytes pass through unchanged and unbuffered (only a *prefix that could still
  become a report* is held back).
- **Encodings to translate** (row = row − `BANNER_ROWS`):
  - **SGR (DECSET 1006):** `ESC [ < Cb ; Cx ; Cy (M|m)` — decimal `Cy` minus
    one. This is what Claude Code and every modern TUI uses.
  - **Legacy/default (DECSET 1000/1002/1003, X10 encoding):**
    `ESC [ M Cb Cx Cy` with single bytes `coord + 32` — decrement the `Cy`
    byte.
  - **UTF-8 (DECSET 1005):** same triplet with UTF-8-encoded coordinates —
    decode, decrement, re-encode. Small and closes the matrix; vt100 already
    tracks it (`MouseProtocolEncoding::Utf8`).
  - **CPR:** `ESC [ Pr ; Pc R` — decimal `Pr` minus one (see M0 audit).
- **Banner-row events are swallowed**, not clamped: a press/release/motion
  with physical row 1 has no grid cell under it. Swallowing the whole report
  keeps the agent's button state consistent (press+release both land on row 1
  in practice; a drag *into* the banner is also swallowed for its row-1
  reports). This also reserves the banner
  row for future click actions (e.g. clicking `[kill:Ctrl-\]`), out of scope
  here.
- **Encoding selection comes from the compositor's parsed state**, not from
  guessing: expose `Compositor::mouse_protocol_encoding()` /
  `mouse_protocol_mode()` (thin wrappers over
  `vt100::Screen::mouse_protocol_mode/encoding`, vt100 0.16.2). Mode `Off` =
  translator bypass (pure passthrough, zero cost for non-mouse agents). The
  translator is (re)configured per stdin chunk — mode changes take effect on
  the next read, which matches how the modes reach the terminal (via the
  next rendered frame) closely enough.

Wiring in `forward_attached_stream` (`attach.rs:864-899`), banner mode only:

1. Existing detach-byte scan (`attach.rs:871`) and kill-byte scan
   (`attach.rs:884`) stay first — `0x1d`/`0x1c` cannot appear inside any of the
   handled encodings (SGR/CPR are printable ASCII; legacy/UTF-8 coordinate
   bytes are ≥ 33), so ordering is safe.
2. Then, instead of `socket_write.write_all(&stdin_buf[..bytes_read])`, feed
   the chunk through the translator and write its output.
3. Non-banner attach keeps the verbatim path (no translator constructed).

No geometry state is needed beyond the `BANNER_ROWS` constant — resize does not
affect the offset.

### M2. Tests, docs, gates

- Unit tests in `crates/terminal` (pure, no PTY): SGR press/release/wheel/
  motion rows decremented; column untouched; row-1 report swallowed; legacy
  X10 and UTF-8 encodings; CPR; a report split across two `push` calls; an
  incomplete prefix held back then completed; garbage that looks like a prefix
  (`ESC [ <` + non-digit) flushed through; translator bypass when mouse mode is
  off.
- Rewrite `attach_banner_non_kill_input_is_forwarded` (`attach.rs:1526`): the
  SGR event must now be forwarded *translated* (`\x1b[<64;7;1M` →
  wheel at physical row 1 = swallowed — pick a row-2 fixture for the forwarded
  case and keep a separate swallowed-banner-row case).
- Knowledge bundle: `docs/knowledge/guides/launcher.md` banner paragraph
  (lines 29-42) gains one sentence: mouse reporting works under the banner and
  coordinates are translated to the agent grid. Run `cargo xtask docs check`.
- Full gate set (fmt, clippy `-D warnings`, tests, release build).

**Done when:** clicking any Claude Code UI element under `banner=true` hits the
element under the pointer; wheel/drag behave identically to `banner=false`;
all listed tests pass.

---

## Track C — in-terminal session control modal

### C0. Scope and UX decisions

A keyboard shortcut, while attached **with the banner/compositor active**,
opens a centered overlay box above the live session. The agent keeps running
underneath; its output keeps feeding the grid. The modal owns stdin while open
— nothing leaks to the PTY.

**Trigger key (decided): repurpose `Ctrl-\`.** Today it kills the session
instantly (`BANNER_KILL_BYTE`, `attach.rs:48`, `handle_banner_input_action`,
`attach.rs:1037`). Instant irreversible kill on a single chord adjacent to
detach was a foot-gun anyway; `Ctrl-\` becomes "open session menu" and *Kill*
moves inside it behind a confirm. One prefix key total; the banner label
changes from `[kill:Ctrl-\]` (`BANNER_KILL_LABEL`, `attach.rs:43`) to
`[menu:Ctrl-\]`, and `BannerInputAction::Kill` becomes `OpenMenu`.

Availability: **banner mode only.** Without the compositor there is no safe way
to draw an overlay over a passthrough byte stream (the agent owns the physical
terminal). Non-banner attach keeps today's behavior; the shortcut does nothing
there (documented).

**Menu items (phase 1, all existing RPCs):**

| Key | Action | RPC / mechanism |
|-----|--------|-----------------|
| `k` | Kill session (→ confirm `y`/`Esc`) | `session.stop` (`send_stop`, `attach.rs:1069`) |
| `d` | Detach | existing detach path (`send_detach` + `AttachStreamEnd::Detached`) |
| `n` | New session in the same worktree | `session.inspect` → `session.new` (below) |
| `r` | Rename session (inline text input) | `session.rename` |
| `Esc` | Close menu | — |

**Menu items (phase 2, needs protocol work — C3, confirmed in scope):**

| Key | Action | RPC |
|-----|--------|-----|
| `f` | Fork session (new session resuming the same native conversation) | new `session.fork` |

**"New session in the same worktree"** needs no new protocol:
`SessionInfo.cwd` (`crates/protocol/src/session.rs:432`) is the session's
worktree checkout, and `SessionNewParams.cwd` (`session.rs:59-61`) launches
in-place in that directory without binding a new worktree (`repo`/`branch`
absent). The path is host-local on **the daemon's** side in both fields, so
this works identically for remote attach. Agent profile: reuse the current
session's `agent` (`SessionInfo.agent`, `session.rs:428`); `cols`/`rows` from
the current effective attach size. On success the modal shows the created
session id (`Result` state below); it does **not** auto-switch the attach —
switching mid-attach is a follow-up (needs a detach + re-run of
`run_attach_once` with a new target; note it, don't build it now).

### C1. Compositor overlay support (`crates/terminal`)

Extend `Compositor` with an overlay layer:

- `set_overlay(Option<OverlayFrame>)` where `OverlayFrame` is a pre-styled
  small screen: a list of lines (title, items, footer) plus a desired size;
  the compositor computes centered geometry, clips to the grid, draws a border
  and padding. Content styling stays plain (reverse-video title, one
  highlighted row) — no color theme work.
- Render pipeline (`render`, `compositor.rs:134`): paint the banner + grid
  diff exactly as today, **then** draw the overlay rows last, every frame. The
  grid diff may repaint cells under the overlay; drawing the overlay after the
  diff within the same buffered frame means no visible flicker.
- Opening and closing the overlay invalidates the diff baseline
  (`self.prev = None`) so the grid underneath repaints fully when the box
  disappears.
- Cursor: hidden while the overlay is open, except in the rename-input state
  (park it at the input cell); restore agent cursor state on close (already
  derived per-frame from the grid in `write_cursor`, `compositor.rs:234` —
  only the open/close transitions need care).
- Unit tests in the compositor style (assert on emitted escape sequences):
  overlay draws centered and clipped; grid diff under the overlay does not
  bleed over it within a frame; closing repaints the covered region; cursor
  hidden/restored.

### C2. Modal state machine + attach-loop wiring

**Pure state machine** (unit-testable, no I/O) — new module in
`crates/terminal` (e.g. `menu.rs`), consistent with the headless/view split
convention:

```
enum MenuState { Closed, Root { selected }, ConfirmKill,
                 RenameInput { buffer }, Busy { label }, Result { message } }
enum MenuEvent  { Key(u8-or-decoded), Tick, RpcDone(...), RpcFailed(...) }
enum MenuEffect { ForwardNothing, RunKill, RunDetach, RunNewSession,
                  RunRename(String), Close }
```

`step(state, event) -> (state, Vec<MenuEffect>)`. Navigation: `j`/`k`/arrows +
`Enter`, direct hotkeys (`k d n r`), `Esc` walks back
(`Result/Confirm/Rename → Root → Closed`). `Busy` ignores input except `Esc`
(which closes the modal but lets the RPC finish). Mouse reports arriving while
open are swallowed (M1's translator already parses them — drop, don't
forward).

**Attach-loop wiring** (`forward_attached_stream`, `attach.rs:814`):

- New `Option<ModalState>`-like field next to `banner`; only constructible
  when `banner.is_some()`.
- stdin arm: after the detach scan, if the modal is open route the chunk into
  the state machine instead of the socket; if closed and the chunk contains
  the trigger byte, split like the existing kill handling
  (`handle_banner_input_action`, `attach.rs:1037` — forward the prefix, open
  the modal, drop the rest of the chunk).
- Effects run as the existing RPCs on the control `client` (they are already
  `async` in the loop: `send_stop`, `send_detach`, plus new
  `send_new_session`, `send_rename` built with `request_with_params` like
  `build_resize_request`, `attach.rs:538`). While an RPC runs the modal shows
  `Busy`; result/error lands in `Result` (errors rendered as text — never
  tear down the attach because a menu action failed).
- Kill keeps its current post-RPC semantics (`AttachStreamEnd::SessionStopped`
  path); detach likewise. New-session/rename return to `Result` with the
  outcome and the attach continues.
- Repaint: any modal state change arms `frame_deadline` (same coalescing as
  banner updates, `attach.rs:909-920`).

### C3. Fork session (protocol ripple — in scope)

"Fork" = a **new** pohunek session whose agent resumes the same native
conversation (Claude: `claude --resume <native_session_id> --fork-session`;
Codex analog via its captured resume metadata), in the same worktree by
default. The daemon already captures native resume metadata per session
(`session.resume` relaunches in-place, `crates/protocol/src/method.rs:110`);
fork is "resume, but into a fresh session id/PTY".

- New method `session.fork`: params `{ session_id, name?, cwd_mode: same|…,
  cols, rows }` → result = new `SessionInfo`. Daemon: validate the source
  session has resumable native metadata (typed error when not), build the
  fork launch command per agent kind, register a new session.
- Full protocol ripple per AGENTS.md: `protocol` → `client` → `daemon` →
  `cli` (new `pohunek fork` command + the modal's `f` entry) → `gui-core`
  (session action) — plus `docs/public-api.md` and the knowledge bundle in the
  same change.
- The modal ships in C2 **without** `f`; C3 adds the entry when the RPC
  exists, so C1/C2 never block on the protocol work.

### C4. Docs, knowledge, gates

- `docs/knowledge/guides/launcher.md`: banner paragraph documents the menu
  chord, the modal actions, and the changed kill flow (kill now confirmed
  inside the menu — a safety-rule-adjacent change, so also check
  `docs/knowledge/safety/` for any "kill" mention).
- `docs/knowledge/concepts/sessions.md`: new-session-from-worktree and (after
  C3) fork semantics.
- `cargo xtask docs check` + the full gate set; C3 additionally updates
  `docs/public-api.md`.

**Done when:** with `banner=true`, the chord opens the modal over a running
Claude session; `k` kills only after confirm; `n` creates a session in the same
worktree and reports its id without disturbing the attach; `r` renames; `Esc`
always returns to the live session with a clean repaint; non-banner attach is
byte-identical to today.

---

## Order & decision points

1. **M1 + M2** — mouse fix (small, self-contained, unblocks modal mouse
   swallowing).
2. **C1** — compositor overlay (pure `crates/terminal`, testable alone).
3. **C2** — modal state machine + attach wiring (phase-1 actions, `Ctrl-\`
   remapped to the menu).
4. **C3** — `session.fork` protocol track + the modal's `f` entry.
5. **C4** — docs/knowledge for the whole track (C2 keys + kill-flow change,
   C3 fork semantics + `docs/public-api.md`).

Operator decisions are resolved (2026-07-05): `Ctrl-\` is remapped to the
menu, and C3 (fork) is in scope. Track M resolves **drag-into-banner
semantics** by swallowing row-1 motion reports instead of clamping them to the
agent grid; the behavior is pinned by terminal input tests.
