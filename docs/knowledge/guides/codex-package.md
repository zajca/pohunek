---
type: Guide
id: guide/codex-package
title: Codex runtime package
description: The official Codex runtime package source (Codex 0.x releases from 0.160.0, verified on 0.164.0), the only source of the codex runtime, what it declares and keeps equal to the built-in Codex runtime of release 0.34.2, what was verified against a real Codex, and the gaps found.
source_kind: manual
intents: [setup, update, debug, help]
---

# Codex runtime package

The Codex package (`runtime-packages/codex`: `runtime.toml` and `detect.toml`,
nothing else) is the only source of the Codex runtime: the daemon compiles no
Codex in. It is the package form of the built-in Codex runtime that releases up
to 0.34.x compiled in. Its compatibility lock and the screens captured from a real Codex
are in `compat/codex/`. See [runtime packages](runtime-packages.md) for the
generic commands and [Pi](pi-package.md) for the package this one mirrors.

## Status: official package in the release catalog; no built-in

`codex` is a reserved runtime id. A package serves it only with official trust:
a `--catalog` install whose signed catalog entry binds the package id, runtime
id and digest, verified against the host's catalog trust anchor (see [runtime
packages](runtime-packages.md)). An install trusted by `--sha256` is refused
with `package_runtime_not_claimable`. Since release 0.34.2 every Linux daemon
archive ships the package as `runtime/packages/codex.tar.zst` with a signed
`runtime/runtime-catalog.json` whose signer matches the shipped trust anchor;
[installing official packages](runtime-packages.md#install-official-packages-from-a-release)
has the procedure. The Codex hook handler (`codex-hook-v1`) and its reporter
template stay compiled into core; the descriptor and detection manifest come
only from the package.

The release catalog lists the package for the Linux x86_64 glibc and musl
targets, and for Apple Silicon (aarch64-apple-darwin).

On a host without the package nothing serves `codex`:

- `pohunek session new --agent codex` fails with `agent_profile_not_found`, and
  `pohunek integration install --agent codex` with `runtime_not_installed`; both
  hints name `pohunek plugin install runtime/packages/codex.tar.zst --catalog
  runtime/runtime-catalog.json` (run from the extracted release archive).
- `pohunek integration status|doctor|uninstall --agent codex` still work: the
  core-owned Codex hook handler keeps the config home of the earlier built-in
  (`CODEX_HOME`, else `~/.codex`), so hooks pohunek installed there are reported
  and removed (only pohunek-owned entries) without the package. `--profile <name>` and `--all-profiles`
  reach the homes of host profiles over `codex` too, each through the
  profile's own `[env]` (`CODEX_HOME`), although such a profile does not
  resolve without the package. A bare `integration status` or `doctor`
  without `--agent` covers only served runtimes, so name `--agent codex` to see
  leftover Codex hooks.
- A host profile with `base = "codex"` and no `package`/`digest` keys fails with
  `runtime_not_installed`; its hint names the install and then `pohunek plugin
  profile migrate <name>`, which pins the profile to the installed package.
- A `codex` in the foreground of a shell session, or started outside pohunek, is
  not attributed to Codex: process recognition and transcript observation read
  the package's `detect.toml` and `[transcripts]` table.
- A session the built-in Codex of an earlier release launched is refused on
  resume, fork and every interaction (input, attach, resize, rename, metadata)
  with `runtime_builtin_removed`, whose hint names the install; only stop and
  remove still work. With the package installed, the package adopts the session,
  live or stopped, and resume and fork run through it when it honors the
  session's recovery contract (same native reference kind, hook-reported
  reference; fork also needs fork support). A session whose contract the package
  cannot honor answers `runtime_served_by_package`, and no command rebinds the
  session to another runtime source. `pohunekd upgrade-preflight` reports an adoptable
  session as `adoptable`, and a session that no package adopts as
  `would_lose_recovery` (`runtime_source_not_served`); without the package its
  detail says every interaction is refused until the package is installed.

## What the package declares

Every fact below equals the built-in Codex descriptor release 0.34.2 shipped,
except the version probe and the transcript tree (`[transcripts]`), which that
descriptor does not declare. The package tests compare the package with golden
copies of that descriptor and its detection manifest in
`compat/codex/builtin-v0.34/` (`runtime.toml`, `detect.toml`).

- Launch: `codex` with the first prompt as a trailing positional argument;
  bracketed paste followed by a separate Enter 150 ms later for later input.
- Readiness: `ready = "manifest_idle"`. Codex enables bracketed paste long
  before its composer reads input, and its startup update popup (#1114) would
  take a prompt released by the paste signal or by a stale idle title, so the
  initial prompt waits for the manifest's winning evidence to be idle from the
  terminal title or the screen. See
  [runtime packages](runtime-packages.md#declaring-input-readiness).
- Version: `version_probe = { parser = "semver-line-v1", args = ["--version"],
  min = "0.160.0", below = "1.0.0", line = "codex-cli {version}" }`. `codex
  --version` prints `codex-cli 0.164.0` on standard output; the probe reads only
  that first line, so the `WARNING: proceeding, even though we could not create
  PATH aliases` line Codex writes to standard error when its home is under a
  temporary directory is ignored. A pre-release (`codex-cli 0.161.0-rc.1`) or
  any other wording is unsupported. The range is a lower bound: `min` is the
  previous verified release, whose captures stay in `compat/codex/screens/n-1`,
  and `below` is the next major, because hosts update the Codex CLI
  independently of pohunek and a refused launch breaks every Codex session.
  Releases above 0.164.0 are accepted by policy, not verified.
- Native reference: `strategy = "hook"`; resume is `codex resume <id>`; fork is
  unsupported.
- Integration: handler `codex-hook-v1` with hook schema `identity-subagent-v1`.
  The package names the compiled handler and schema; the hook reporter scripts
  stay core-owned, so the archive carries no asset.
- Config home: `CODEX_HOME`, else `~/.codex`.
- Detection (`detect.toml`): process matchers are the kernel name `codex` or a
  command line whose program is `codex`. Codex reports its state through the
  terminal title, so the idle, working and blocked rules read the title:
  a title that starts with a braille spinner and a space, followed by a thread
  or project name rather than the `|` separator, means `working` (the
  `<spinner> | <project>` title of an idle unnamed thread's title generation
  asserts no state: the idle rule skips every spinner title); a title that
  contains `Action Required` (Codex writes `[ ! ] Action Required | <dir>`,
  alternating `[ . ]`) means `blocked` and outranks `working`; any other
  non-blank title means `idle`, read from the absence of a spinner and of
  `Action Required` rather than from a title of its own. Screen rules cover
  prompts that block the turn: the folder trust dialog at the top of the
  screen, the startup update popup (`startup_update_prompt`, below),
  the confirmation lines Codex prints after its last prompt
  marker (`press enter to confirm or esc to cancel`, `enter to submit answer`,
  `enter to submit all`) and the weaker `[y/n]`-style prompts
  anywhere in the recent screen.
- Program status: the manifest declares `[program_status] app = "codex"`.
  Codex 0.163.0 and later write OSC 7501 root records
  (`state=idle|working|blocked|done|error`, `app=codex`, `kind` with
  `blocked`) whenever stdout is a terminal, so the package never answers a
  probe (no `answer_probe`) and a session needs no setup. The detector holds
  the latest record and arbitrates it with the title and screen rules (see
  [runtime packages](runtime-packages.md)): a visible dialog beats a
  `working`, `done`, `idle` or `error` record, a `blocked` record blocks, a
  held `done` or `idle` beats a stale spinner title, and the record's
  `working` stands over an idle title. `done` turns into `idle` on the next
  key press; both read idle, so that change is no transition. `error` reads
  idle. Codex 0.160.0 (recorded: no records) and 0.161.x to 0.162.x (not run; no
  records according to the Codex source) read from the title and the screen
  alone. A record never releases the initial prompt: `manifest_idle`
  still waits for the title or the screen.
- **Background terminals.** Codex reports `working` while a background
  terminal runs, also after the turn ended. The rule `background_terminals_idle`
  (priority 650, above `weak_blocker`; `overrides_program_status`, evaluated only while a record is held) reads idle for the footer
  `N background terminal(s) running · /ps to view · /stop to close` when the
  working row of a running turn is not on screen, so a dev server left running
  does not keep the turn open. The footer is truncated, not wrapped, at narrow
  widths (`1 background termi` at 20 and 30 columns); the rule matches
  `N background term`. While a turn runs the footer joins the working row and
  the row's `(Ns • esc to interrupt)` group gates the rule off. A screen
  with an idle title, no row and no footer reads `working` from the record
  (synthetic scenario). A turn that is sent while the server has not acknowledged
  it yet (the stub holds the response before `response.created`) reads
  `working` from the first moment on 0.164.0
  (`a_real_codex_reads_working_before_the_server_acknowledges_the_turn`). A goal
  that continues between turns would look like the synthetic screen from the
  Codex source; it was not recorded on 0.164.0.
- **MCP startup.** The Codex source treats MCP startup as running work
  (title spinner, `Starting MCP servers` row, `working` record) before any
  turn; the recorded startup sequence of 0.164.0 shows `idle`, `working`,
  `idle`, but no MCP server was configured, so the window is not recorded.
  Readiness is unaffected: a record never releases the initial prompt, and
  `manifest_idle` reads the title and the screen.
- **Known limits.** A forged record can set working or idle when no dialog is
  visible, as forged titles and screen text can. After a daemon adoption whose
  output ring was evicted the record is lost until Codex writes the next one;
  a same-owner manifest refresh drops the held record the same way. Under GNU
  Screen (`STY` set) the Codex 0.164 source writes ASCII spinner glyphs in the
  title (not recorded); the record is unaffected.

## Verified against a real Codex 0.164.0

The screens in `compat/codex/screens/` (named screens at 100 columns, widths 20
to 200 in `widths/`, each with the OSC title Codex had set in a `.title` file)
were captured from a real interactive Codex 0.164.0 in a PTY with a fresh
`CODEX_HOME`, a loopback Responses stub as the model provider and no credential
(the 0.164.0 runs recorded in `compat/codex/compatibility-lock.json` used a network
namespace with only loopback; the recapture check below did not). They cover the idle
screen, a turn held before its first output (`Working (0s • esc to
interrupt)`), a streaming reply, the screen after a turn, a tool approval
prompt (`Would you like to run the following command?`) and, in
`background/`, a background terminal that outlives its turn. The
`*.program-status` files beside them are the ordered, de-duplicated OSC 7501
root records of the same runs (`capture_the_real_codex_screens` and
`capture_the_real_codex_background_terminal_screens` write the screens and the
sequences; `POHUNEK_CODEX_CAPTURE_DIR` names the output directory, and the
second test writes its screens to `background/` below it). The capture run
saw Codex enter the alternate screen (`ESC [ ? 1049 h`); the committed fact is
`alternate-screen.fact`, which `codex_enters_the_alternate_screen` pins; only a
recapture refreshes it. The question prompt, the sign-in screen, the first-run folder dialog,
the startup dialogs and the update popup could not be recaptured on 0.164.0
(no stub marker reaches them, or no newer release exists to announce); they are
the captures of 0.160.0 in `compat/codex/screens/n-1`, `startup-dialogs` and
`update-popup`, as are the 0.160.0 versions of every screen above.
`crates/cli/tests/codex_package.rs` classifies every one of them, of both
releases, through the package manifest. After a turn Codex 0.164.0 shows no
selection popup with the hermetic configuration, and the next input reaches the
composer.

- Title rules: idle, working and blocked are read from the title at every
  captured width; the screen text has no working or idle rule, apart from the
  background-terminal footer above.
- Screen rules: the 0.164.0 approval prompt is blocked from the screen text
  alone at every captured width except 30, where the question and the
  confirmation line wrap over several rows, so no single-line phrase matches;
  there only the title and the record say blocked. The 0.160.0 approval and question prompts are blocked
  from the screen text alone at 30 columns and wider.
- `codex resume <id>` against a fresh home restores the conversation from
  Codex's own session store (checked by hand; the id is the one in
  `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`).
- Hook trust: the records pohunek's integration installer writes
  (`[hooks.state."<hooks.json>:<event>:0:0"] trusted_hash`) are accepted, and the
  SessionStart hook reports the conversation id to the daemon.

## Gaps found

- **The startup update popup holds the screen at startup.** When Codex 0.160.0
  finds a newer release at startup, it draws `Update available · <old> →
  <new>`, the release-notes line, `› 1. Update now (runs npm install -g
  @openai/codex)`, `2. Skip` and `3. Skip until next version` over the whole
  screen, roughly a second after the provisional composer appeared. The popup
  sets no terminal title. Because Codex enables bracketed paste long before,
  `bracketed_paste` readiness released the initial prompt into the popup,
  where the paste is ignored and the separate Enter can select the
  highlighted update action while the composer never sees the prompt. The
  package declares `ready = "manifest_idle"`. Its `startup_update_prompt` rule
  reads the popup as a visible blocker from the top sixteen non-empty screen lines, above the
  title's idle rule, so a prompt is refused (nothing written, the session
  rolled back) while the popup holds. After observing the blocker, cached
  idle evidence cannot release input during a clear or partial redraw. A
  fresh idle title received after the popup disappears can release it, even
  if the value repeats; title arrival is ordered against screen updates
  within each PTY chunk. The rule is verified against captures at 80, 100
  and 160 columns (`compat/codex/screens/update-popup/`); a title left over
  from before the popup is only ever synthetic — the measured popup set no
  title. The additional visible startup dialogs below use the same strict
  blocker/freshness behavior.
- **Visible login and incompatible-settings dialogs hold initial input.**
  Package 1.2.2 recognizes the captured Codex 0.160.0 login selection at
  80/160 columns by its welcome heading, all three sign-in/API-key choices
  and continue hint. API-key entry requires the welcome and entry headings,
  storage hint, input-box heading and both save/back hints. The incompatible
  background-server-settings dialog requires its heading, shared-settings
  hint and all three run/restart/cancel choices. These visible blockers
  outrank an idle title; no credentials or model calls are needed to test them.
- **The folder dialog is blocked from 20 columns.** Codex 0.160.0 shows `Trust
  this folder? Codex can read, edit, and run files here…` with `1. Trust and
  continue` and `2. Back to Agent Command Center` when the working directory
  carries a project-local `.codex/config.toml`. The `folder_trust_prompt`
  requires `Folder access` at the beginning of the top 32 nonempty lines,
  the question and both complete choices, allowing line wrapping. Real
  captures cover 20/30/80/160 columns. The older `Do you trust the contents of
  this directory?` wording, `allow command?` and `yes (y)` appear in no release
  since 0.160.0 and have no rule. Quoted transcripts and incomplete dialogs do
  not establish these blockers.
- **These rules recognize visible dialogs, not completed startup.** The held
  dialogs set no title; a stale idle title in regressions is synthetic.
  `manifest_idle` still permits title evidence without a positive initialized
  composer. Provisional version/CWD/placeholder frames cannot prove startup
  completion; the separate strict screen-policy proposal is
  [#1298](https://github.com/zajca/pohunek/issues/1298), awaiting owner scheduling.
  This package update changes the `pohunek.runtime.codex` digest; after release,
  host profiles pinned to the old digest are repinned during upgrade.
- **Hooks run in a descendant process.** Codex 0.160.0 runs its hooks from a
  `codex app-server --listen unix:// --managed-daemon` process below the
  launched `codex`: a direct child of the native binary, which is itself a child
  of the Node launcher an npm install starts. The session worker accepts a launch
  claim from the launch process itself and from the `codex app-server` helper
  (a process named for the provider, started directly by the launch process,
  whose second argument is `app-server`; an independent `codex` child never
  qualifies; see "Which process may report the
  conversation id" in [sessions](../concepts/sessions.md)), so the app-server's
  conversation id becomes `native_session_id` and `session resume` runs `codex
  resume <id>`. A real-Codex test pins this
  (`a_real_codex_reports_hooks_from_a_descendant_of_the_launched_process_and_resume_has_its_reference`):
  it waits for `native_session_id`, checks the reporter descends from the launch
  process, stops the session, resumes it and checks the reference is unchanged.
  Codex reports a subagent through its own `SubagentStart` hook, which the
  reporter sends as a subagent claim and never as a reference, so a subagent id
  is not promoted; the first reference stays, and a later different id from the
  app-server does not replace it. A resumed or reloaded Codex child sends
  `SubagentStop` without a preceding `SubagentStart`, so for Codex a stop with
  no recorded start records a completed subagent. Codex fires `SubagentStop`
  once per completed child turn, so a stop on a finished child is another
  completed turn: the row takes a new revision and finish time (follow-up
  tasks and resumed children show up when their turn ends; the rule is in
  [sessions](../concepts/sessions.md)).
  The subagent hook inputs and sequences of 0.164.0 are committed as fixtures:
  `compat/codex/subagent-hooks.json` holds a start and a stop input with their
  transcript labels (the start names the child's own transcript; the stop names
  the parent's, and the child's in `agent_transcript_path`; no path or message
  crosses the hook boundary), and `compat/codex/subagent-hook-sequences.json`
  holds, per flow (spawn, follow-up through `send_input` and `followup_task`,
  resume after close, an interrupted turn, a close of a running child), the hook
  events in order and their key sets. A resumed child fires no `SubagentStart`,
  an interrupted or closed child fires no `SubagentStop` (its row stays
  `running` until a later stop or the end of the runtime, and counts against
  Codex's own 64 running children only), and a `multi_agent_v2`
  child fires no `UserPromptSubmit`. The opt-in test
  `a_real_codex_produces_the_committed_subagent_hook_sequences` records the
  flows again from the real `codex` through the loopback Responses stub and
  compares them with the file; it refuses a `codex` that is not the locked
  release, and the `codex-package` CI job runs it. Run it alone with
  `POHUNEK_CODEX_E2E=1 cargo test -p pohunek-cli --test all -- --include-ignored
  --test-threads 1 --nocapture codex_subagent_sequences::`; after a deliberate
  change of the release or the flows, merge the observed recording it prints
  into the file (its `format`, `tools` and `description` fields are
  hand-written). The app-server also outlives the TUI when it is
  detached; the fixture's process guard stops every process of the fixture,
  including after a failed assertion. A Codex state hook action (`session`,
  `subagent-start`, `subagent-stop`) that fails to deliver its report still
  exits 0 and appends one payload-free line per reason (`provider` `codex`, the
  action and a reason code) to the hook failure trace `pohunek-hooks.jsonl` in
  the pohunek log directory (see the
  [debug-session-runtime](../runbooks/debug-session-runtime.md) runbook).
- **Process forms.** The helper process has kernel name `codex` too, so the
  matchers accept it. An npm install (`@openai/codex@0.164.0`) launches the tree
  `node …/@openai/codex/bin/codex.js` (the launched process), the native
  `…/vendor/x86_64-unknown-linux-musl/bin/codex` below it, and the `codex
  app-server` below that; a native package install has no launcher. The
  launcher matches neither process pattern, the native binary and the
  app-server match both, and a test pins these forms. Launch, detection and the
  reported conversation id behave the same with both installs.

## Not verified

The compaction and retry screens, the `PermissionRequest`, `Stop` and subagent
hooks, a Codex with `tui.animations = false` or a `tui.terminal_title` without
`activity` (the record still reports), terminals narrower than 20
columns, macOS and Windows, a real-network run (the real-Codex tests switch off
update checks, analytics, plugin sync and connectors but do not prove the
absence of egress).

## How the package is kept honest

- `crates/cli/tests/codex_package.rs` always runs: it builds the directory
  twice and requires identical bytes, parses it with the daemon's install
  parser, compares every descriptor fact and the manifest with the v0.34.2
  built-in Codex goldens in `compat/codex/builtin-v0.34/` (as parsed
  definitions, as TOML data, and by feeding both manifests every captured frame,
  title and synthetic dialog). Those comparisons leave out the package's
  package-only folder, update, login, API-key and settings rules and the
  corresponding captures; separate tests read every captured
  folder dialog frame, and every popup capture with and without a synthetic
  stale idle title, with the package manifest. The startup-dialog JSON fixtures
  retain exact hexadecimal PTY prefixes and 40 rendered rows from isolated
  Codex 0.160.0 captures; API-key entry is empty. Daemon API scenarios replay
  those bytes through a real PTY, require refusal/rollback with zero task
  bytes while held (with and without a stale title), then explicitly dismiss
  the dialog and require exactly one delivery after fresh idle evidence.
  It requires the
  supported range to equal `compat/codex/compatibility-lock.json`, reads the
  version banner, runs the title, screen and process rules, and asserts the
  `--sha256` install refusal above.
- The daemon-backed tests install the built archive through `pohunek plugin
  install --catalog`, the way a host with a catalog trust anchor does: the test
  process generates a throwaway signing key, gives the daemon an anchor that
  trusts it (`crates/test-support/src/catalog.rs`, feature `catalog`), and
  signs a catalog entry binding the package id, the runtime id `codex` and the
  archive digest.
  The key never leaves the process and no real key or catalog is involved. The
  package then serves `codex` with official trust, profiles carry its
  `package`/`digest` pin, and a launch runs the package's version probe on the
  executable the profile names. An always-running test drives that probe with
  throwaway `codex` scripts: the older minor, the next major, a pre-release, a
  different banner wording, unreadable output and a failing probe are refused
  with `agent_runtime_unsupported`, whose message and `pohunek doctor` detail
  name the parsed release and the supported range, or that the output did not
  parse; the locked banner and a later 0.x release (`codex-cli 0.162.1`)
  launch. Two more
  always-running tests start from a host without the package and a fake `codex`
  of the locked release on `PATH`: the bare runtime, its integration and an
  unpinned `base = "codex"` profile are refused with the install hint (and the
  migration for the profile), and after the catalog install the bare runtime
  launches pinned to the package and the migrated profile launches. A third
  installs the hooks into the hermetic `~/.codex` through the package,
  uninstalls the package, and checks that `integration status`, `doctor` and
  `uninstall --agent codex` still report and remove them while `integration
  install` is refused. A fourth does the same for two host profiles over
  `codex` with their own `CODEX_HOME`, first pinned and then rewritten
  without a pin as an earlier release wrote them: `--profile` removes one
  profile's hooks and `--all-profiles` the rest, with no home left behind.
- The fixture owns a process guard: on success and on unwind it kills every
  process whose executable, working directory or environment value lies below
  the fixture's root (the session worker, Codex and the detached app-server)
  before the directories are removed. Two tests pin the guard, including a panic
  after a detached process started.
- Three `#[ignore]`d tests drive a real `codex` through the installed package
  with a fresh `CODEX_HOME` and the loopback Responses stub
  (`crates/cli/tests/support/responses_stub.rs`): the banner against the probe
  template, launch and detection through the daemon (the inventory shows the
  package's probe accepted the real release; screens and titles are read through
  the package manifest as well), and the approval prompt. The hook trust comes
  from `pohunek integration install --agent codex --profile …`, which resolves
  the profile's `CODEX_HOME` so the real home is never touched. Run them with
  `POHUNEK_CODEX_E2E=1 cargo test -p pohunek-cli --test all --
  --include-ignored --test-threads 1 codex_package::`; without the variable and a `codex` on
  `PATH` they fail by name instead of skipping.
- The `codex-package` CI job installs exactly the locked release from npm
  (`@openai/codex@0.164.0`), builds the archive with `cargo xtask package build`
  and runs that file.

Update procedure for a new Codex release: install it from the locked npm tree,
move the current captures to `compat/codex/screens/n-1` (the older set is
deleted and `min` raised to the previous release), rerun the real-Codex tests,
then recapture the screens and program-status sequences with
`POHUNEK_CODEX_CAPTURE_DIR` set to the absolute path of `compat/codex/screens`
(`POHUNEK_CODEX_E2E=1 cargo test -p pohunek-cli --test all -- --include-ignored
--test-threads 1 codex_package::capture_the_real`). The two capture tests also
serialise on a lock beside their fixed recording root `/dev/shm/ph-codex`. A
recapture of 0.164.0 reproduced the committed layout (including `background/`
and `alternate-screen.fact`); only the clock times, elapsed seconds and the
spinner glyph of the captured rows and titles differed. Move the lock and the
descriptor's `below` together, and bump the package `version`.
