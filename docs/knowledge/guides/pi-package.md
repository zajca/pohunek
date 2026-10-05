---
type: Guide
id: guide/pi-package
title: Pi runtime package
description: Install and operate the official Pi coding agent runtime package (Pi 1.0.x), including what was verified against a real Pi, how resume and fork work, and what fails closed.
source_kind: manual
intents: [setup, update, debug, help]
---

# Pi runtime package

Pi (`pi`, the Pi coding agent) runs under pohunek through a runtime package, not
through compiled daemon code. The package source is `runtime-packages/pi`
(`runtime.toml` descriptor and `detect.toml` manifest, nothing else); its
compatibility lock and captured screens are in `compat/pi/`. See
[runtime packages](runtime-packages.md) for the generic install, trust, and
selection commands.

## Install

Pi must be on the daemon's `PATH` (`npm install --global
@earendil-works/pi-coding-agent@<release>`, Node 22.19 or newer). Build the
archive from a checkout and install it with the digest it prints:

```bash
cargo xtask package build runtime-packages/pi --output pi.tar.zst
pohunek plugin install ./pi.tar.zst --sha256 sha256:<digest> --yes
pohunek host inspect local --json
pohunek session new --agent pi --input "Refactor the parser."
```

The trust is local and explicit: the digest you supply authorizes those exact
bytes, and the package is never official. The runtime id `pi` is not reserved,
so no catalog is needed.

## What the package declares

- Launch: `pi` with the first prompt as a positional argument, and bracketed
  paste followed by a separate Enter 150 ms later for later input.
- Version: `pi --version` prints exactly the release on standard output
  (extension noise goes to standard error, which the probe discards). The
  descriptor accepts `[1.0.0, 1.1.0)`; any other release, or output that is not
  one `MAJOR.MINOR.PATCH` line, refuses the launch with
  `agent_runtime_unsupported`. `host.inspect` shows `version` and `supported`.
  The probe runs under the `PATH` the launch will see (the daemon's forwarded
  environment overridden by the profile's `PATH`), so a profile that selects a
  Node installation is probed with that Node; every other variable is isolated.
- Native reference: `strategy = "assigned"`. The daemon generates a UUID, starts
  `pi --session-id <id>` (Pi creates the conversation with that id), resumes with
  `pi --session <id>`, and forks with `pi --fork <id>`. No integration hook is
  involved.
- Existence check: before resume or fork the daemon looks for a regular file
  named `*_<id>.jsonl` in a project directory below
  `$PI_CODING_AGENT_DIR/sessions/` (default `~/.pi/agent/sessions/`). Pi writes
  `sessions/--<cwd with / as ->--/<ISO time with : and . as ->_<id>.jsonl`, and
  only after the first model reply.
- Detection: both rules read the editor frame anchored at the bottom of the
  screen: an upper border, the draft, a plain lower border, then one to four
  footer lines. A busy Pi writes `── `, a braille spinner and its message
  (`Working`, `Compacting context...`, `Auto-compacting...`, `Retrying (n/m) in
  Ns...`) into the upper border, cut to the terminal width, so its closing rule
  can be a single `─`; the `── <spinner>` start alone marks a busy border at any
  width, and it means `working`. A plain upper border (a rule of 20 or more `─`,
  or a `↑ N more` / `↓ N more` marker between runs of four or more) means
  `idle`. Terminals narrower than 20 columns are never read as idle. Draft and
  transcript text never decides: a rule above the editor cannot stand in for the
  upper border, a line that starts with `─` is text, and a draft line that
  starts `── <spinner>` or is itself a complete plain border cannot be told from
  the frame. The idle, busy, compaction, retry, scrolled-draft and
  transcript-rule screens in `compat/pi/screens/` (widths 20 to 200 in
  `widths/`) were rendered by a real Pi. No blocked signal (extension dialogs,
  login) and no overlay (model or session selector) was exercised, so no
  `blocked` rule exists, an unrecognized screen keeps the byte-activity
  fallback, and a replacement editor or an extension that changes the spinner is
  not recognised as busy. Process matchers accept the kernel name `pi` (Pi retitles its
  process shortly after start) and `node` running
  `…/coding-agent/dist/bundle/cli.js` for the first moments.

## Operating notes

- A session that never received a model reply has no session file; `resume` is
  refused with `agent_native_reference_missing` before any worker starts, as is
  a resume after the file was deleted.
- A forked session holds no reference of its own and is not resumable.
- A Pi configured to store sessions elsewhere (`--session-dir`,
  `PI_CODING_AGENT_SESSION_DIR`, `sessionDir`) fails recovery closed; set
  `PI_CODING_AGENT_DIR` in the host profile's `[env]` to move the whole agent
  directory instead.
- Pin a profile to the installed digest (`pohunek plugin profile migrate`) so a
  later `plugin update` cannot change an existing profile's package. A profile
  `[env]` may set `PI_OFFLINE`, `PI_SKIP_VERSION_CHECK` and `PI_TELEMETRY`; never
  put provider keys into docs or logs.

## How the package is kept honest

- `crates/cli/tests/pi_package.rs` always runs: it builds the package directory
  twice and requires identical bytes, parses it with the daemon's install
  parser, requires the descriptor's supported range to equal
  `compat/pi/compatibility-lock.json`, runs the manifest on real Pi screens
  (`compat/pi/screens/`) and process forms, and runs the existence check on
  Pi's real file layout.
- The same file holds two `#[ignore]`d tests that drive a real `pi` through an
  installed package: launch, idle and working detection, input, stop, resume,
  fork, and refusal without a session file. A loopback chat-completions stub
  stands in for the model, so no provider, credential or network is involved.
  Run them with `POHUNEK_PI_E2E=1 cargo test -p pohunek-cli --test pi_package --
  --ignored`; the variable and a `pi` on `PATH` are required, otherwise the run
  fails by name instead of skipping.
- The `pi-package` CI job installs exactly the locked release from npm, builds
  the archive with `cargo xtask package build`, and runs that file.

Update procedure for a new Pi release: install it, rerun the real-Pi tests, move
the lock (`release`, and `supported` if the range changes) and the descriptor's
`min`/`below` together, refresh the captured screens if the interface changed,
and bump the package `version`.
