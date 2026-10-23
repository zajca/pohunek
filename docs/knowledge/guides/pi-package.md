---
type: Guide
id: guide/pi-package
title: Pi runtime package
description: Install and operate the official Pi coding agent runtime package (Pi 1.x from 1.0.0, verified on 1.4.0 with the 1.0.2 screens kept as the previous release), including what was verified against a real Pi, how detection reads dialogs and selectors, how resume and fork work, where sessions may live, and what fails closed.
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
@earendil-works/pi-coding-agent@<release>`, Node 22.19 or newer).

Since release 0.34.2 every Linux daemon archive ships the Pi package as
`runtime/packages/pi.tar.zst` with the signed `runtime/runtime-catalog.json`.
Installing it with `--catalog` makes it an official package; see [installing
official packages](runtime-packages.md#install-official-packages-from-a-release).

From a checkout, build the archive and install it with the digest it prints:

```bash
cargo xtask package build runtime-packages/pi --output pi.tar.zst
pohunek plugin install ./pi.tar.zst --sha256 sha256:<digest> --yes
pohunek host inspect local --json
pohunek session new --agent pi --input "Refactor the parser."
```

That trust is local and explicit: the digest you supply authorizes those exact
bytes, and such an install is never official. The runtime id `pi` is not
reserved, so either way of installing serves it.

## What the package declares

- Launch: `pi` with the first prompt as a positional argument, and bracketed
  paste followed by a separate Enter 150 ms later for later input.
- Readiness: `ready = "screen_idle"`. Pi enables bracketed paste about
  130 ms after spawn (measured on 1.0.2), before it draws any screen, so a host profile's initial
  input (written into the terminal, not passed as an argument) waits for the
  idle editor frame, the `idle_editor_rule` of the detection manifest. A
  session whose editor frame does not show within the host's
  `initial_startup_grace_ms` fails `session.new` with
  `session_input_not_ready` and is rolled back; the initial input is not
  written. The manifest recognises the editor frame from 20 columns up, so
  such a launch into a narrower terminal is always refused.
- Version: `pi --version` prints exactly the release on standard output
  (extension noise goes to standard error, which the probe discards). The
  descriptor accepts `[1.0.0, 2.0.0)`, the verified range's lower bound up to
  the next major (1.4.0 is verified; the 1.0.2 captures stay in `compat/pi/screens/n-1/`
  as the previous verified release); any other release, or output that is not
  one `MAJOR.MINOR.PATCH` line, refuses the launch with
  `agent_runtime_unsupported`. `host.inspect` shows `version` and `supported`.
  The probe runs under the `PATH` the launch will see (the daemon's forwarded
  environment overridden by the profile's `PATH`), so a profile that selects a
  Node installation is probed with that Node; every other variable is isolated.
- Program status: the manifest declares `[program_status] app = "pi"` with
  `answer_probe = true`. Pi 1.1.0 and later print an OSC 7501 probe at start and
  report `working`, `done`, `idle`, `error`, `blocked` (kind `permission`,
  `question` or `auth`) and `clear` only after the terminal answers; the session
  worker answers each probe once, so a Pi launched by pohunek reports, and its
  records rather than the screen decide the activity (`state_source`
  `osc_progress`).
  A visible dialog the screen rules see still wins over a non-blocked record:
  Pi's project-trust question is recorded on 1.4.0 and reads `blocked`. The
  other own confirms (missing cwd, session import) are covered by screens from
  earlier releases only; a confirm no rule matches reads as Pi's record says. Pi 1.0.x prints no probe and is read from the screen alone.
  Known limits: a dialog drawn with `ctx.ui.custom()` has no screen rule and
  reads as Pi's record says (working during a run); the branch-summary busy
  border reads `idle`, as Pi declares it; `error` reads as `idle`; a Pi nested
  under a shell session gets no probe reply and stays screen-only; a profile
  that sets `PI_PROGRAM_STATUS=0` also gives screen-only detection. A client's
  own reply to the probe can reach Pi before the worker's reply when the worker
  has to wait behind a paced input; Pi then reads "no program status" for that
  process. After an adoption with an evicted output ring, or after a package
  refresh, the held record is lost until Pi's next record. Anything that can write
  to the terminal can forge a record: it can set working or idle when no visible
  blocker is on screen, as it can forge screen text today.
- Native reference: `strategy = "assigned"` with `assign_fork = true`. The
  daemon generates a UUID, starts `pi --session-id <id>` (Pi creates the
  conversation with that id), resumes with `pi --session <id>`, and forks with
  `pi --fork <id> --session-id <fork id>` under a second UUID it generates for
  the fork. No integration hook is involved.
- Existence check: before resume or fork the daemon looks for a regular file
  named `*_<id>.jsonl` in a project directory below
  `$PI_CODING_AGENT_DIR/sessions/` (default `~/.pi/agent/sessions/`). Pi writes
  `sessions/--<cwd with / as ->--/<ISO time with : and . as ->_<id>.jsonl`, and
  only after the first model reply. When the launch environment sets
  `PI_CODING_AGENT_SESSION_DIR` (from the profile `[env]` or the daemon's
  forwarded environment), Pi keeps the files directly in that directory and the
  check looks there instead; the value must be an absolute path.
- Session-dir overrides: Pi picks its session directory as `--session-dir`,
  else `PI_CODING_AGENT_SESSION_DIR`, else the `sessionDir` setting of
  `$PI_CODING_AGENT_DIR/settings.json` or the project's `.pi/settings.json`.
  The daemon follows the variable, and refuses a launch it could not recover
  from with `agent_native_store_relocated` before any worker starts: a profile
  whose `args` pass `--session-dir`, a relative variable, or (while the
  variable is unset) a `sessionDir` set in either settings file, or a settings
  path that is not a readable JSON file (a FIFO or a directory there is
  refused without waiting on it). At launch the hint names
  `PI_CODING_AGENT_SESSION_DIR`: setting it in the profile `[env]` to the
  directory the setting names satisfies both Pi and the check, because Pi
  follows the variable over the setting; the flag wins over the variable, so
  its hint says to remove it. Resume and fork run the same refusal against the
  session's arguments and working directory first. The launch passed that
  refusal, so a refusal there means something changed since: a `sessionDir`
  setting appeared, or the variable the launch had is gone (a daemon restarted
  without forwarding it, or a profile edit accepted with
  `--accept-profile-change`) and an older setting now wins. The hint says to
  undo the change: remove the setting (or make an unreadable settings file
  readable JSON), or restore `PI_CODING_AGENT_SESSION_DIR` to the absolute
  path the session was launched with; an edited profile then needs
  `--accept-profile-change`. Pointing the variable at a new directory would
  not find the conversation.
- Detection: every rule reads the editor frame anchored at the bottom of the
  screen: an upper border, a body, a plain lower border, then one to four
  footer lines. A busy Pi writes `── `, a braille spinner and its message
  (`Working`, `Compacting context...`, `Auto-compacting...`, `Retrying (n/m) in
  Ns...`) into the upper border, cut to the terminal width, so its closing rule
  can be a single `─`; the `── <spinner>` start alone marks a busy border at any
  width, and it means `working`. A plain upper border (a rule of 20 or more `─`,
  or a `↑ N more` / `↓ N more` marker between runs of four or more) around the
  draft means `idle`. Terminals narrower than 20 columns are never read as idle.
  Body and footer lines are never borders, so the lower border is the last
  border on the screen and the working and idle rules cannot both match;
  `scripts/pi-detect-regex` generates and checks the patterns.
- Dialogs and selectors: Pi draws them in place of the editor, between plain
  borders and without the busy border even while a turn waits on them, so the
  body decides. `blocked` (priority above `working`) is a body that holds the
  key hint or title of a prompt waiting for an answer, whoever opened it:
  - an extension's select, confirm, input or editor dialog, also when a
    permission check raises it mid-turn;
  - Pi's own confirms drawn the same way: the project trust prompt at startup,
    the missing session cwd prompt, "Summarize branch?" in `/tree` and "Import
    session";
  - the `/trust` selector;
  - every login step (method, provider list, API key, browser or pasted-code
    login).

  The owner-opened ones (`/trust`, `/login`, `/tree`'s summary question,
  import) read `blocked` and notify too: each holds the session on a question,
  and their screens cannot be told from an extension's dialog. Hint words may
  wrap onto padded continuation lines, the startup prompts draw no footer, and
  a prompt taller than the terminal may push its upper border off the screen;
  all three still read `blocked`. A prompt so tall that Pi clips its bottom
  rows loses its lower border, and in some cases its key hint; it shows no
  frame and matches no rule. A body that holds the two rows a selector list draws (a cursor row
  starting `→ `, ` → ` or `› `, and a row indented by exactly two spaces: another
  item, the `(n/m)` counter or the selected item's details) withholds `idle`,
  so the model, session, tree, fork and settings selectors (opened by the
  owner, asking nothing) match no rule and keep the byte-activity fallback,
  which publishes `working` from process output until the editor returns. A blocked session raises the "Agent needs attention"
  notification and the `agent_blocked` turn event, and ends `session wait
  --activity blocked`. The package's unrestricted input policy still accepts a
  plain `session input` while Pi is blocked (and Pi's submit delay rules out
  the waited form), so an orchestrator waits for `idle` or `blocked` before it
  sends and leaves a blocked session to the owner.
- What was verified: the idle, busy, compaction, retry, scrolled-draft and
  transcript-rule screens in `compat/pi/screens/` (widths 20 to 200 in
  `widths/`), and the dialog, `/trust`, login and selector screens in
  `compat/pi/screens/overlays/` (each at widths 20 to 200, the unframed
  `[y/N]` question at 100 only), were rendered by a real Pi driven with real
  key presses. Not covered, and matching no rule: a dialog an
  extension draws with `ctx.ui.custom()`, a replacement editor or an extension
  that changes the spinner, and Pi's plain-text `Fork this session into
  current directory? [y/N]` question before its interface starts (Pi asks it
  only when the session belongs to another directory; pohunek relaunches a
  session in its own). Draft and transcript text: transcript lines never
  decide; a quote (`> text`) or a single line starting with a cursor is draft
  text and reads `idle`; a draft that holds both selector rows (a line
  starting `→ ` or `› ` and one indented by two spaces) withholds `idle`, so
  the session stays `working` until the draft no longer holds both rows; a
  draft line that reproduces a dialog's key hint reads `blocked`; and a draft line that starts
  `── <spinner>` or is itself a complete plain border cannot be told from the
  frame. Process matchers accept the kernel name `pi` (Pi
  retitles its process shortly after start) and `node` running
  `…/coding-agent/dist/bundle/cli.js` for the first moments.

## Operating notes

- A session that never received a model reply has no session file; `resume` is
  refused with `agent_native_reference_missing` before any worker starts, as is
  a resume after the file was deleted.
- A forked session holds its own assigned reference from the start: Pi writes
  the fork's file `<ISO time>_<fork id>.jsonl` at once (its header names the
  source file as `parentSession`), before any model turn, so the fork resumes
  with `pi --session <fork id>` after a stop or a daemon restart. Pi refuses a
  fork id that already names a session; the daemon generates a fresh UUID for
  every fork. Package version 1.0.1 and later declare this; a host with 1.0.0
  installed gets it with `pohunek plugin update pohunek.runtime.pi <archive>`
  (installed beside 1.0.0 and selected). A session launched from 1.0.0, or from
  a profile still pinned to its digest, keeps the spec it was launched with, so
  its forks hold no reference and are not resumable.
- Package version 1.0.2 adds the `blocked` and overlay-aware idle rules and the
  session-dir handling. A session keeps the existence check it was launched
  with: one launched from 1.0.1 or earlier searches only the agent directory's
  store and refuses nothing at launch, so it fails closed at recovery when its
  Pi kept the files elsewhere. Detection uses the manifest of the package
  version a session was launched from, so a session launched from 1.0.1 keeps
  reading its dialogs and selectors as `idle`.
- To keep sessions outside the agent directory, set
  `PI_CODING_AGENT_SESSION_DIR` (absolute) in the host profile's `[env]`, or
  set `PI_CODING_AGENT_DIR` to move the whole agent directory; do not pass
  `--session-dir` in profile `args`.
- A stopped session moves to a newer official Pi package version on resume or
  fork when three things hold. The newer version keeps the session's native
  reference check and its `--session-id` assignment. A session launched from a
  profile also needs the profile to pin the newer version
  (`pohunek plugin profile migrate <name> --yes`; `service upgrade` repins a Pi
  profile pinned to an older official version, and leaves an unpinned one). A session launched as the bare `pi` runtime follows the
  selected version. The relaunch is then pinned to the newer digest. A
  version whose check or assignment differs, such as 1.0.2 over a session from
  1.0.1, keeps the session on its own digest.
- `service upgrade` keeps an installed official Pi package current: when it is
  installed from the official catalog, selected and enabled, the upgrade
  installs and selects the release's version and keeps the older one installed.
  It never installs an absent Pi package, never enables or selects one you turned
  off, and keeps a selected version newer than the release's instead of
  downgrading it. The plan reports a deselected package as a gap with the
  `pohunek plugin update pohunek.runtime.pi runtime/packages/pi.tar.zst --catalog
  runtime/runtime-catalog.json --yes` command, and a disabled one with
  `pohunek plugin enable pohunek.runtime.pi --digest <digest>`.
- Pin a profile to the installed digest (`pohunek plugin profile migrate`) so a
  later `plugin update` cannot change an existing profile's package. A profile
  `[env]` may set `PI_OFFLINE`, `PI_SKIP_VERSION_CHECK` and `PI_TELEMETRY`; never
  put provider keys into docs or logs.

## How the package is kept honest

- `crates/cli/tests/pi_package.rs` always runs: it builds the package directory
  twice and requires identical bytes, parses it with the daemon's install
  parser, requires the descriptor's supported range to equal
  `compat/pi/compatibility-lock.json`, runs the manifest on real Pi screens
  (`compat/pi/screens/`, with `overlays/`) and process forms, and runs the
  existence check and the session-dir refusals on Pi's real file layouts.
- The same file holds `#[ignore]`d tests that drive a real `pi` through an
  installed package: launch, idle and working detection, input, stop, resume,
  fork, stop and resume of the fork, refusal without a session file, an
  extension dialog and the project trust prompt read `blocked`, a session kept
  in `PI_CODING_AGENT_SESSION_DIR` resumes, and a `sessionDir` setting or the
  `--session-dir` flag refuses the launch. A loopback chat-completions stub
  stands in for the model, so no provider, credential or network is involved.
  Run them with `POHUNEK_PI_E2E=1 cargo test -p pohunek-cli --test all --
  --ignored pi_package::`; the variable and a `pi` on `PATH` are required, otherwise the run
  fails by name instead of skipping.
- The `pi-package` CI job installs exactly the locked release from npm, builds
  the archive with `cargo xtask package build`, and runs that file.

Update procedure for a new Pi release: stage it (`cargo xtask compat stage-upstream
--runtime pi`), rerun the real-Pi tests, move the lock (`release`, and
`supported` if the range changes) and the descriptor's `min`/`below` together,
record the screens again (`record_the_pi_screens` with `POHUNEK_PI_E2E=1` and
`POHUNEK_PI_CAPTURE_DIR`) and the program-status sequences (`POHUNEK_PI_UPDATE_GOLDENS=1`
with the real-Pi program-status test, written to `compat/pi/screens/sequences/`),
and bump the package `version`. The oldest `n-1/` set is deleted, the current
set moves to `n-1/` and `min` rises to that release; the manifest tests run
over both sets. Screens the recording does not produce (retry, compaction, overflow,
autocompaction, scrolled and six-line drafts, transcript-rule widths, mid-turn
permission, trust selector, login, selector, missing-cwd and fork screens) are
carried over unchanged from the previous release until they are recorded again.
