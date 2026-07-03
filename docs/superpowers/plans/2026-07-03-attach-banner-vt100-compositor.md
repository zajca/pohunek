# Attach banner as a vt100 compositor (works under Codex & Claude Code)

Status: IMPLEMENTED (full scope — shared `pohunek-terminal` crate + compositor)
Date: 2026-07-03
Owner: (attach path, `crates/cli`; screen model `crates/terminal`)

## Implementation note (differs from the original sketch)

The compositor DOES use a scroll region + DEC origin mode — but safely, because
it is now the *only* writer to the physical terminal. `vt100`'s
`rows_formatted`/`rows_diff` bake absolute cursor moves using the grid row
index, so a naive per-row `\x1b[{i+2};1H` offset is not enough on its own. The
client sets a stable scroll region `\x1b[2;Nr` + origin mode `\x1b[?6h` once, and
origin mode offsets *both* the client's positioning and vt100's internal moves
onto physical rows 2..N. This is exactly what the old overlay could not do:
there the agent's raw bytes reached the terminal and reset those margins; here
they are absorbed into the parser. The banner row is drawn with origin mode
briefly disabled.

## Problem

`pohunek attach` streams PTY bytes straight to the local `stdout` — it is a byte
pipe, not a terminal emulator. The old attach banner reserved physical row 1 by
resizing the session PTY to `rows-1`, setting a scroll region `\x1b[2;Nr`
(DECSTBM) plus origin mode `\x1b[?6h` (DECOM), and drawing the banner on row 1.

Full-screen TUI agents (Codex, Claude Code) switch to the *alternate screen
buffer* (`\x1b[?1049h`). Entering the alt screen on the real terminal resets the
scroll margins to the full screen and clears origin mode, so the TUI's
`\x1b[1;1H` lands on physical row 1 and overwrites the banner; the TUI also sets
its own scroll region and repaints on its own cadence. Reasserting the viewport
after every PTY chunk (commit `f8fa46b`) raced the TUI's repaints and produced
flicker/corruption, so `c510652` disabled the overlay entirely
(`ATTACH_BANNER_OVERLAY_ENABLED = false`).

Conclusion: passthrough + escape-sequence reassertion cannot host a persistent
banner above an alt-screen TUI. The client must own what reaches the physical
terminal.

## Chosen approach: client-side vt100 compositor (tmux model)

The client parses the incoming PTY stream into its own `vt100` screen grid and
re-renders a composite (banner row + grid) itself. The TUI's raw control
sequences (`?1049h`, its scroll regions, its absolute cursor moves) are absorbed
by the parser and never touch the physical terminal — so nothing can fight the
banner.

`vt100 0.16` is already a workspace dependency (`Cargo.toml:64`, used by
`crates/daemon/src/detect/screen.rs`) and exposes exactly the primitives needed
(verified in the crate source):

- `Parser::process(&[u8])` — feed PTY bytes; handles alt-screen transparently.
- `Screen::rows_formatted(start, width) -> impl Iterator<Item = Vec<u8>>` — per
  row bytes; **caller positions the cursor per row**, which cleanly offsets the
  grid down by one physical row with no DECSTBM/DECOM tricks.
- `Screen::rows_diff(prev, start, width)` — incremental repaint (less flicker).
- `Screen::cursor_position()`, `hide_cursor()`, `alternate_screen()` — correct
  final cursor placement.
- `Screen::set_size(rows, cols)` — resize on `SIGWINCH`.

### Rendering model

- Client holds `vt100::Parser` sized `(rows - 1, cols)`; row 1 is the banner.
- Incoming PTY bytes → `parser.process(bytes)` (no direct stdout write).
- Frame render (coalesced/throttled, NOT per chunk):
  1. Banner: `\x1b[1;1H` + reverse-video + `render_banner_text(cols, snapshot)`
     + clear-to-eol + SGR reset (reuse existing text builder verbatim).
  2. Grid: for row `i` in `0..rows-1`, emit `\x1b[{i+2};1H` then the
     `rows_formatted`/`rows_diff` bytes for that row.
  3. Cursor: if `hide_cursor()` → `\x1b[?25l`; else place at
     `\x1b[{cur_row+2};{cur_col+1}H`.
- `SIGWINCH`: `parser.set_size(rows-1, cols)`, force a full (non-diff) repaint,
  send the existing resize request to the daemon.
- Detach / EOF / kill: reset SGR, restore cursor, clear scroll region if any.

Throttling: coalesce bytes arriving within a short window (~8–16 ms) and render
once, so a burst of PTY output produces one frame. Keep a `prev` screen clone
for `rows_diff`; fall back to `rows_formatted` on the first frame and after
resize.

## Work breakdown

### Scope A — compositor in `crates/cli` (minimum)
1. Add `vt100 = { workspace = true }` to `crates/cli/Cargo.toml`.
2. New module `crates/cli/src/commands/attach/compositor.rs`:
   - `struct AttachCompositor { parser, prev: Option<Screen>, size, banner }`
   - `fn feed(&mut self, &[u8])`, `fn render_frame(&mut self) -> Vec<u8>`,
     `fn resize(&mut self, cols, rows)`, `fn reset() -> Vec<u8>`.
   - Pure/unit-testable: given fed bytes + snapshot, assert emitted frame
     positions banner on row 1 and grid on rows 2..N, incl. an alt-screen
     transcript (`?1049h` + absolute moves) proving the banner survives.
3. Rewire `forward_attached_stream` (`attach.rs:841`):
   - Replace the direct `stdout.write_all(&socket_buf..)` passthrough with
     `compositor.feed(...)` + throttled `render_frame()` writes.
   - Delete the DECSTBM/DECOM overlay helpers that are now obsolete
     (`render_banner_viewport_frame`, `render_banner_viewport_repaint_frame`,
     `repaint_banner`, `enter_banner_viewport`, `reset_banner_frame`, the
     `?6h/?6l` machinery) and their tests.
   - Keep: `AttachBannerSnapshot`, `render_banner_text`, snapshot updates via
     `spawn_banner_updates`, the Ctrl-\ kill shortcut, reconnect logic.
4. Re-enable the feature: remove `ATTACH_BANNER_OVERLAY_ENABLED`; gate purely on
   `banner=true` + a terminal with `rows >= MIN_ROWS_WITH_BANNER`.
5. `banner_interval_seconds` becomes the render throttle / live-refresh cadence
   (repurpose, do not invent a new key).

### Scope B — shared VT crate (cleaner, recommended if time allows)
Extract `ScreenTracker`/vt100 usage into a small `crates/vt` (or
`crates/terminal`) crate so client and daemon share one screen model. Daemon's
`detect/screen.rs` re-exports from it. Slightly larger workspace change; avoids
two divergent vt100 wrappers.

## Docs / ripple (mandatory per CLAUDE.md)
- `docs/knowledge/guides/launcher.md`: rewrite the "overlay disabled" paragraph
  to describe the working compositor banner (works under Codex/Claude Code,
  reserves row 1, `banner_interval_seconds` = refresh cadence, Ctrl-\ kill).
- `crates/cli/src/commands/setup.rs`: update the `launcher.conf` comment block
  (currently says "disabled at runtime for TUI safety").
- Check `docs/public-api.md` and `docs/knowledge/assistant/source-map.md` for
  any attach/banner surface references; update if present.
- Re-run `cargo xtask docs check`.

## Verification gates (CI is source of truth)
- Rust guidelines: read `.agents/rust-guidelines/11_universal_guidelines.md`
  (+ correctness/application files) before editing any `.rs`; update the
  `// Rust guideline compliant <date>` marker.
- Build, test, lint from AGENTS.md; clippy is `-D warnings`.
- New compositor unit tests incl. the alt-screen survival transcript.
- Manual: attach to a Codex session and a Claude Code session; confirm the
  banner stays on row 1 across full-screen repaints, resize, and detach cleanly
  restores the terminal.

## Risks
- SGR/attribute carry-over between rows: `rows_formatted` handles per-row attrs;
  verify no color bleed at row boundaries in tests.
- Wide/CJK glyphs: vt100 already models these (see `screen.rs` slice tests).
- Latency vs. passthrough: mitigated by coalescing; matches the tmux model.
- Bracketed paste / mouse / application-cursor modes originating from the TUI
  are now mediated by vt100 — confirm `application_cursor()` and input mode
  passthrough for keys still reach the PTY correctly (input path is unchanged;
  only output is composited).
