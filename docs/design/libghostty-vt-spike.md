# libghostty-vt spike report

Status: measured. Recommendation: **defer**. Nothing in this report ships; the
product keeps `vt100` as its screen model. Tracking: issue
[#2538](https://github.com/zajca/pohunek/issues/2538).

The spike answers one question: should Pohunek replace `vt100` (0.16.2, as
`TerminalTracker`, `ScreenTracker` and the CLI `Compositor` use it) with
[libghostty-vt](https://github.com/ghostty-org/ghostty) 1.0.2 as its terminal
screen model? The code is in `spikes/libghostty-vt/` (a standalone Cargo
workspace outside the root workspace, see [development.md](../development.md)).
The generated evidence is committed next to it in `spikes/libghostty-vt/report/`
so it outlives workflow artifacts.

## Summary

- libghostty-vt passes every spec assertion that `vt100` passes (59 of 59
  against 49 of 59) and parses 1.4 to 2.9 times faster, at +1.8 MiB of
  stripped `pohunek-sessiond` on Linux and 1.5 to 1.6 minutes of cold Zig build
  locally (2.8 to 2.9 minutes on the CI runners). It links statically on musl, adds no C++ runtime symbol, stays under
  the glibc ceiling and links on macOS arm64 with `minos 14.0`.
- It is not adoptable yet because adoption criterion 2 fails: the measured
  wins over `vt100` are mostly properties `vt100` cannot observe, scrollback
  (which `vt100` with a scrollback buffer also passes) and emoji widths that
  the one recorded agent trace does not exercise.
- The repaint that attach depends on is not a libghostty defect. With the
  formatter's default options it differs on 5 of 13 traces; all but one of
  those traces are closed by two formatter options (`UNWRAP` on,
  `TRIM` off), which the harness now uses. One trace is left (`emoji-width`):
  a screen whose rows were written under different mode 2027 settings cannot
  be replayed as one text stream. Criterion 3 therefore holds on 12 of 13
  traces, and the owner decides whether the 13th is in scope.
- The product decision is: keep `vt100`, keep `vte` as the print observer,
  do not start the adoption sequence (A1 to A5 of the plan), and revisit when
  the open items below are closed.

## Method

**Models.** Three models replay every fixture through the same `GridModel`
interface:

| Model | What it is |
|---|---|
| `vt100` | `pohunek_terminal::TerminalTracker` plus a second `vt100::Parser::new(rows, cols, 0)` for cells; the dump fails if the two disagree on the visible text. This is what the worker runs. |
| `vt100-scrollback` | The same parser with `vt100_scrollback_rows` (10 000) of scrollback. It shows how far `vt100` alone gets with scrollback enabled. |
| `libghostty` | `HeadlessTerminal` over the 1.0.2 static library, built with Kitty graphics compiled out, and no reply, clipboard-read or title-report callback installed. |

**Fixtures.** 13 recorded PTY traces (`fixtures/*.trace`), each with a sidecar
`*.expect.toml` of spec-derived assertions. An assertion cites a neutral source;
a model's output is never saved as a golden and the model under test is never
a source. Width assertions that no standard fixes (emoji ZWJ sequences, VS16,
flags) state their width method and the mode 2027 state and form their own
group.

| Trace | Content | Assertions | Source |
|---|---|---|---|
| `claude` | Claude Code 2.1.309 against a loopback stub, 30x100: start, one prompt and reply, exit | 7 | xterm control sequences, ECMA-48 |
| `combining` | one mark, stacked marks, a mark after a wide character | 6 | UAX 11, UAX 29 |
| `emoji-width` | ZWJ family, VS15/VS16 heart, regional-indicator flag | 7 (width-method group) | UAX 11, UAX 29 |
| `hyperlink-wrap` | OSC 8 link spanning a wrap | 5 | OSC 8 specification, xterm |
| `kitty-keyboard` | `CSI > 1 u`, `CSI ? u`, `CSI < u` | 3 | Kitty keyboard protocol |
| `osc52` | OSC 52 write and `?` read | 3 | xterm |
| `program-status` | Pohunek's OSC 7501 probe | 1 (`pohunek-contract`, cites `crates/terminal/src/program_status.rs`) | repository |
| `resize-alt-screen` | resize while the alternate screen is active | 5 | xterm |
| `resize-wrap` | 4x20, shrink to 4x10, grow back, soft-wrapped line | 4 | xterm |
| `scroll-region` | DECSTBM | 6 | xterm |
| `scrollback` | three screen heights of output, then a resize | 3 | xterm |
| `title-progress` | OSC 0/2 and OSC 9;4 | 3 | xterm, Windows Terminal progress sequences |
| `wide-chars` | East Asian wide characters, one at the last column | 6 | UAX 11, ECMA-48 |

**Repaint round trip.** Each model's repaint (`TerminalSnapshot::ansi` for
`vt100`, the VT formatter with all extras for libghostty, with the options of
`ghostty_repaint_*` in `config/model.toml`) is fed to a fresh instance of the
same model, created at the size of the source, and the two final screens are
diffed. OSC 52 writes
and progress are not screen state and are left out.

**Measurements.** `scripts/measure` builds `size-probe` and `throughput` cold
in release mode with and without the feature, per target, and records sizes,
wall times, link checks and the median of 5 `throughput` runs per model and
workload. The workloads are the recorded agent trace replayed 16 384 times
(37.5 MiB) and a seeded synthetic flood of 64 MiB fed in 4 KiB chunks. The
`vt100` subject is `TerminalTracker`; `vt100-parser` is the bare parser;
`vt100-scrollback` is the bare parser with scrollback; `libghostty` is the
headless terminal. `pohunek-sessiond` was measured once on an unmerged local
branch that links libghostty into `TerminalTracker`.

Numbers marked local were taken on one workstation (AMD Ryzen 9 9900X, 24
logical CPUs, Linux 7.3.5, rustc 1.99.0). Numbers marked CI come from the spike
workflow run [45804222731](https://github.com/zajca/pohunek/actions/runs/45804222731)
(`report/measurements.json`: ubuntu-24.04 AMD EPYC 9V74 with 4 logical CPUs,
macos-15 Apple M1 with 3). Shared CI runners are slower and noisier, so every
number is indicative.

## D2: assertions and differences

Generated from the tree of the commit that adds this file; the spike workflow
run of the pull request repeats the report on CI. Generated by
`cargo run --release --features libghostty --manifest-path spikes/libghostty-vt/Cargo.toml --bin spike-report -- --models vt100,vt100-scrollback,libghostty --out spikes/libghostty-vt/report`
(`report/report.md` and `report/report.json`).

| Model | spec | width-method | pohunek-contract | all |
|---|---|---|---|---|
| `vt100` | 44 / 51 | 4 / 7 | 1 / 1 | 49 / 59 |
| `vt100-scrollback` | 45 / 51 | 4 / 7 | 1 / 1 | 50 / 59 |
| `libghostty` | 51 / 51 | 7 / 7 | 1 / 1 | 59 / 59 |

The 10 assertions `vt100` fails and libghostty passes:

| Assertion | Why `vt100` fails | Counts toward adoption? |
|---|---|---|
| `hyperlink-wrap`: 3 OSC 8 assertions | cannot observe hyperlinks | no, not a motivating case |
| `kitty-keyboard`: 2 flag assertions | cannot observe the Kitty keyboard flags | no |
| `osc52`: write observed | cannot observe OSC 52 writes | no |
| `scrollback`: `scrolled-rows-retained` | no scrollback | no: `vt100-scrollback` passes it |
| `emoji-width`: `family-cluster`, `vs16-heart-cluster`, `flag-per-codepoint` | per-codepoint cell layout | only with agent TUI parity (criterion 2) |

Final-screen differences between `vt100` and libghostty (`report.md`, pairwise
section): `emoji-width` 23, `resize-wrap` 89, `resize-alt-screen` 19,
`scrollback` 2 (retained rows), `wide-chars` 1 (a soft-wrap flag), `claude` 1
(the title is `Some("")` in `vt100` and `None` in libghostty). The other
traces have none. Two of these have no assertion:

- `resize-wrap`: libghostty reflows soft-wrapped lines on resize and `vt100`
  does not. No neutral specification says which is right, so the fixture
  asserts only the facts before the resize and the sizes after it.
- `resize-alt-screen`: after `CSI ? 1049 h` libghostty keeps the cursor row
  where the primary screen left it and prints on row 1; `vt100` prints on row
  0. The xterm text for mode 1049 saves the cursor and clears the alternate
  screen without a cursor move, which matches libghostty; the fixture has no
  assertion for the position.

**Repaint round trip** (differences after feeding the repaint to a fresh
model of the same kind):

| Trace | `vt100` | `vt100-scrollback` | `libghostty` |
|---|---|---|---|
| `claude` | 1 (title) | 1 (title) | 0 |
| `emoji-width` | 0 | 0 | 17 |
| `hyperlink-wrap` | 0 | 0 | 0 |
| `resize-wrap` | 0 | 1 | 0 |
| `scrollback` | 0 | 1 | 0 |
| `title-progress` | 1 (title) | 1 (title) | 1 (title) |
| `wide-chars` | 0 | 0 | 0 |
| other 6 traces | 0 | 0 | 0 |

The title differences are not repaint failures: neither model's ANSI repaint
carries the title, which `TerminalSnapshot` transports as its own field. Without
them `vt100` has no difference, and libghostty differs on 1 of 13 traces. The
`vt100` figure is a weaker claim than it looks: its model has no scrollback,
so there is nothing to lose, and `vt100-scrollback` keeps its history out of
`TerminalSnapshot::ansi`, which is why it differs on `scrollback` and
`resize-wrap`. The libghostty repaint carries the history rows.

**Root cause of the libghostty repaint differences.** The formatter was first
run with all extras and its default options (`UNWRAP` off, `TRIM` on). Each
option was then varied alone and together; mode 2027 was enabled in the fresh
instance by writing `CSI ? 2027 h` before the repaint. Traces that differ:

| Formatter options | `emoji-width` | `hyperlink-wrap` | `resize-wrap` | `scrollback` | `wide-chars` | Traces |
|---|---|---|---|---|---|---|
| default (`UNWRAP` off, `TRIM` on) | 17 | 1 | 49 | 12 | 1 | 5 |
| `UNWRAP` on | 17 | 0 | 50 | 12 | 0 | 3 |
| `TRIM` off | 17 | 1 | 0 | 0 | 1 | 3 |
| `UNWRAP` on, `TRIM` off | 17 | 0 | 0 | 0 | 0 | 1 |

Writing `CSI ? 2027 h` to the fresh instance changes no count in any row of
this table. The fresh instance has a source-sized grid in every variant.

- `hyperlink-wrap`, `wide-chars` (row wrap flag): with `UNWRAP` off the
  formatter writes every screen row as its own line, so the fresh terminal
  does not mark the soft-wrapped rows as wrapped. `UNWRAP` on writes a
  wrapped line as one line, which the fresh terminal wraps again at the same
  width. Closed by configuration.
- `resize-wrap`, `scrollback` (retained rows and shifted cells): with `TRIM`
  on the formatter drops blank rows at the end of the output, so the fresh
  terminal lays history and screen out one row off (12 against 11 retained
  rows). The formatter header documents that VT output with trimming off
  keeps those rows "so the output fills the same rows as the screen".
  Closed by configuration.
- `emoji-width` (17 differences, rows 0, 3 and 5): the trace writes the same
  text under `CSI ? 2027 l` and `CSI ? 2027 h`. The rows written with the mode
  off hold one cell per code point, the rows written with it on hold one
  cluster. A cell does not record the width method it was written under, and
  the formatter emits the text of every row plus one final mode state
  (extra `MODES`), so rows whose cells differ between the methods come back with the wrong
  cells. The rows written under the final mode state (1, 2 and 6) round-trip.
  This is a property of a text repaint, not a defect in the library, and no
  formatter option addresses it. A screen that keeps one width method round
  trips; a program that switches the mode between rows of one screen does not.

The configured repaint is larger, 5.6 KB on an 8x20 screen against 0.1 KB for
`vt100`, because the extras are all on (the formatter writes the palette, modes and tab stops).

**Agent trace.** The recorded Claude Code session replays with identical cells in
`vt100` and libghostty; the only difference is the title. It is one short trace
(2.5 KB, a prompt and a reply, ends on the primary screen) and has no wide or
emoji content, so it neither confirms nor refutes the emoji width differences.

## D3: measurements

### Library and link

| | glibc x86_64 | musl x86_64 | macOS arm64 |
|---|---|---|---|
| `libghostty-vt.a` (local build) | 5 481 062 B | 5 477 614 B | not built locally |
| `libghostty-vt.a` (CI build) | 5 479 846 B | 5 476 414 B | 2 978 968 B |
| `zig build`, cold cache, local | 98.4 s | 88.7 s | |
| `zig build`, cold cache, CI ubuntu-24.04 | 175.2 s | 168.8 s | 175.1 s (cross build on Linux) |
| Link | no duplicate symbol; 0 C++ runtime symbols added; highest `GLIBC_` 2.36 (`size-probe`), 2.39 (`pohunek-sessiond`, also without the feature) | static-pie; 0 C++ runtime symbols added; no duplicate symbol | no linker warning; `minos 14.0`, Xcode 16.4, `macos-15` |

The CI figures come from workflow run
[45804222731](https://github.com/zajca/pohunek/actions/runs/45804222731) (head `7868d5165` of this pull request);
the link results were identical in run
[45794204949](https://github.com/zajca/pohunek/actions/runs/45794204949) (head
`53f7b1619` of the S1 to S5 pull request). The local musl links use
Rust's self-contained musl linking; the workflow links with `musl-gcc` like
`release-build.yml`. The glibc ceiling is 2.41.

### `size-probe` (the model surface linked into a small Rust binary, local)

| Target | Stripped without | Stripped with | Delta | Unstripped delta |
|---|---|---|---|---|
| glibc x86_64 | 869 216 B | 2 865 720 B | +1 996 504 B (1.90 MiB) | +2 064 992 B |
| musl x86_64 | 942 880 B | 2 945 976 B | +2 003 096 B (1.91 MiB) | |

Cold `cargo build --release` of the probe takes 4.6 to 4.8 s locally and 12.2 to
12.4 s on the Linux CI runners (24.4 to 24.6 s on macos-15) with and without the
feature: the library is prebuilt, so its cost is the Zig build above. On macOS
arm64 the stripped delta is +1 010 464 B (0.96 MiB); the CI deltas on Linux are
+1 996 360 B (glibc) and +2 003 096 B (musl).

### `pohunek-sessiond` (one measurement, local only)

Branch `2538-s6s7-sessiond-measure-local-only`, commit
`cff827ece91c93e69510dc65fbff3e7a3264a664` on base `077589c0e`, local to the
workstation above, never pushed (`report/sessiond.json`). It links libghostty
into `TerminalTracker` and keeps `vt100` for `Compositor` and `ScreenTracker`.

| Target | Stripped without | Stripped with | Delta | Cold release build |
|---|---|---|---|---|
| glibc x86_64 | 7 747 888 B | 9 655 624 B | +1 907 736 B (1.82 MiB) | 21 s without, 25 s with |
| musl x86_64 | 7 545 288 B | 9 458 272 B | +1 912 984 B (1.82 MiB) | 22 s without, 22 s with |

macOS arm64 was not built for `pohunek-sessiond`.

### Throughput and memory

Local: median of 5 runs, MiB/s of PTY bytes, glibc x86_64 (musl in parentheses). The
resident delta is the first run in a fresh process.

| Subject | Agent trace | Flood 64 MiB | Resident delta (flood) |
|---|---|---|---|
| `vt100` (`TerminalTracker`) | 180.4 (166.0) | 155.2 (137.1) | 0.1 MiB |
| `vt100-parser` | 215.1 (207.9) | 164.1 (144.2) | 0.1 MiB |
| `vt100-scrollback`, 10 000 rows | 183.9 (178.0) | 156.9 (133.9) | 31.3 MiB |
| `libghostty`, 8 MiB scrollback limit | 461.4 (448.0) | 221.3 (228.6) | 8.9 MiB |

Throughput of libghostty is 2.6x (2.7x) on the agent trace and 1.4x (1.7x) on
the flood relative to `TerminalTracker`. These are single-process, single-core
parse speeds without PTY or network.

On the CI runners (median of 5, MiB/s, agent trace / flood):

| Subject | glibc x86_64 | musl x86_64 | macOS arm64 |
|---|---|---|---|
| `vt100` | 111.8 / 98.3 | 101.8 / 77.9 | 67.4 / 77.3 |
| `vt100-parser` | 135.2 / 110.9 | 121.2 / 88.8 | 85.6 / 82.0 |
| `vt100-scrollback` | 122.4 / 104.7 | 115.6 / 95.5 | 88.3 / 77.8 |
| `libghostty` | 264.8 / 154.0 | 254.8 / 155.2 | 196.4 / 138.3 |

libghostty is 2.4x, 2.5x and 2.9x `TerminalTracker` on the agent trace and 1.6x,
2.0x and 1.8x on the flood. The resident delta on Linux is 9.1 MiB for
libghostty and 31.3 to 31.9 MiB for `vt100-scrollback`; the macOS runner does not
report it.

## Adoption criteria

| # | Criterion | Result | Evidence |
|---|---|---|---|
| 1 | No assertion regressions | **Met.** Every assertion `vt100` passes, libghostty passes. | 59 / 59 against 49 / 59 |
| 2 | Measured wins on the motivating cases | **Not met.** Wide and emoji: 3 width-method wins, but only count together with criterion 4, and the agent trace has no such content. Combining marks: no win, both pass. Soft-wrap reflow: libghostty reflows, but no neutral assertion exists, so it is a difference, not a measured win. Scrollback: libghostty passes, and so does `vt100-scrollback`, so scrollback alone does not justify adoption. | D2 tables |
| 3 | Repaint fidelity | **Met on 12 of 13 traces, not on `emoji-width`.** Zero differences on every fixture is required. The five differing traces of the default formatter options: four are closed by `UNWRAP` on and `TRIM` off (the harness uses them). The remaining 17 differences come from rows written under different mode 2027 settings in one screen, which a text repaint cannot carry. The owner decides whether that case is in scope. | repaint table, formatter options table |
| 4 | Agent TUI parity | **Met for the one trace**: the visible text is identical; the only difference is the title. | `claude` pairwise section |
| 5 | Linking | **Met.** musl static, no C++ runtime symbol, no duplicate symbol, glibc 2.39 at most (ceiling 2.41), macOS arm64 with `minos 14.0` and no warning. | link table, runs 45794204949 and 45804222731 |
| 6 | Cost thresholds (owner, 2026-11-26) | **Met on the measured targets; open for macOS arm64 `pohunek-sessiond` size.** Size +1.82 MiB per Linux target (limit +3 MiB). Cold build: Zig build 1.5 to 1.6 min locally and 2.8 to 2.9 min on the CI runners (macOS arm64: 2.9 min, cross build on Linux), plus 0 to 4 s of Cargo, under +5 min per target. Throughput better than `vt100` on the agent trace and the flood on every target. macOS arm64 `size-probe` +0.96 MiB; `pohunek-sessiond` size not measured. | tables above |

The second `vte` parser can go only if libghostty's dirty-row reporting covers
every positive-width print observation on the agent trace. That check is not
implemented in the harness, so the answer is unknown and `vte` stays.

## Recommendation: defer

Not adopt: criterion 2 fails, and criterion 3 holds on 12 of 13 traces only,
with a documented residual (`emoji-width`). The repaint that attach correctness
rests on is not what blocks adoption: the formatter round-trips with `UNWRAP`
on and `TRIM` off, and keeps the history that `TerminalSnapshot::ansi` of
`vt100-scrollback` does not. Not drop: criteria 1, 4 and 5 pass, criterion 6 passes on the measured targets, the throughput and
size cost are well inside the thresholds, and libghostty fixes things `vt100`
cannot do (hyperlink, Kitty keyboard and OSC 52 state, grapheme clusters under
mode 2027, retained scrollback in a byte-bounded buffer: 9 MiB resident at an 8 MiB
limit, against 31 MiB for 10 000 `vt100` rows after the flood).

Revisit when these are done, in this order:

1. Decide whether a screen that switches mode 2027 between rows must
   round-trip (criterion 3, `emoji-width`); if so, a repaint built from the
   cells and their widths, not from the formatter text, is the only way.
2. Add a second real agent trace (plan decision O2) and wide or emoji
   content to the agent recording, so criteria 2 and 4 can speak to emoji
   widths.
3. Add the print-observation check (`vte` replacement) if removing the second
   parser matters.
4. Build the local measurement branch on a Mac for the macOS
   `pohunek-sessiond` size: the workflow measures `size-probe` only.
5. Add a spec-based assertion for the cursor after mode 1049 and decide what
   `vt100` should do about it independently of this spike.

If scrollback on reattach is the only wanted gain, enabling scrollback in the
current model is a separate, smaller change: `vt100-scrollback` passes the
same assertion for about 31 MiB per 10 000 rows. It does not solve the
repaint: its `TerminalSnapshot::ansi` carries no history, and its round trip
already differs on `scrollback` and `resize-wrap` (1 each).

## Not measured

- `pohunek-sessiond` on macOS arm64: no workflow step builds it, so the
  size threshold of criterion 6 is open for macOS.
- A native Zig build time on macos-15: the macOS library is cross-built on
  Linux, so 175.1 s is the cross-build time.
- Resident memory on macOS (the runner does not report it).
- Other formatter options (selection, plain or HTML output) as a repaint.
- The `vte` print-observation coverage.
- A second agent runtime, and any trace with emoji content from a real agent.
- Memory of libghostty at a scrollback equal in rows to the `vt100` setting,
  and CPU time under real PTY load.

## Promotion

The plan's promotion table applies: with a `defer` result, only a write-up of
the comparison method and its measured results, presented as an evaluation, is
honest. Listing Pohunek in an external libghostty project list is not, because
Pohunek does not use the library. No external post and no list entry is part
of this change; both need the owner's explicit decision.

## Reproduce

```bash
python3 -I spikes/libghostty-vt/scripts/build-libghostty-vt x86_64-unknown-linux-gnu OUT --fetch
LIBGHOSTTY_VT_DIR=OUT cargo run --release --features libghostty \
  --manifest-path spikes/libghostty-vt/Cargo.toml --bin spike-report -- \
  --models vt100,vt100-scrollback,libghostty --out report-out
python3 -I spikes/libghostty-vt/scripts/measure --target x86_64-unknown-linux-gnu \
  --ghostty-dir OUT --runner LABEL --scratch EMPTY_DIR --out measure.json
```

`report.json` and `report.md` are deterministic: two runs over the committed
fixtures produce byte-identical files. `measurements.json` (CI runners) and `sessiond.json` (workstation)
are numbers of the host named in them; the local figures in the tables above
are not committed as a file.
