---
type: Guide
id: guide/claude-package
title: Claude Code runtime package
description: The official Claude Code runtime package source (Claude Code 2.1.x from 2.1.289), the only source of the claude runtime, what it declares, how its detection differs from the built-in Claude runtime of release 0.34.2, what was verified against a real Claude Code, and the detection limits that remain.
source_kind: manual
intents: [setup, update, debug, help]
---

# Claude Code runtime package

The Claude package (`runtime-packages/claude`: `runtime.toml` and `detect.toml`,
nothing else) is the only source of the Claude Code runtime: the daemon compiles
no Claude in. It is the package form of the built-in Claude runtime that
releases up to 0.34.x compiled in. Its compatibility lock and the screens captured from a
real Claude Code are in `compat/claude/`. See [runtime
packages](runtime-packages.md) for the generic commands and the [Codex
package](codex-package.md) for the package this one mirrors.

## Status: official package in the release catalog; no built-in

`claude` is a reserved runtime id. A package serves it only with official trust:
a `--catalog` install whose signed catalog entry binds the package id, runtime
id and digest, verified against the host's catalog trust anchor (see [runtime
packages](runtime-packages.md)). An install trusted by `--sha256` is refused
with `package_runtime_not_claimable`. Since release 0.34.2 every Linux daemon
archive ships the package as `runtime/packages/claude.tar.zst` with a signed
`runtime/runtime-catalog.json` whose signer matches the shipped trust anchor;
[installing official packages](runtime-packages.md#install-official-packages-from-a-release)
has the procedure. The Claude hook handler (`claude-hook-v1`) and its reporter
template stay compiled into core; the descriptor and detection manifest come
only from the package.

The release catalog lists the package for the Linux x86_64 glibc and musl
targets, and for Apple Silicon (aarch64-apple-darwin).

On a host without the package nothing serves `claude`:

- `pohunek session new --agent claude` fails with `agent_profile_not_found`,
  and `pohunek integration install --agent claude` with `runtime_not_installed`;
  both hints name `pohunek plugin install runtime/packages/claude.tar.zst
  --catalog runtime/runtime-catalog.json` (run from the extracted release
  archive). Install the package first, then the hooks with `pohunek integration
  install --agent claude`; the package takes the native session id from them.
- `pohunek integration status|doctor|uninstall --agent claude` still work: the
  core-owned Claude hook handler keeps the config home of the earlier built-in
  (`CLAUDE_CONFIG_DIR`, else `~/.claude`), so hooks pohunek installed there are
  reported and removed (only pohunek-owned entries) without the package. `--profile <name>` and
  `--all-profiles` reach the homes of host profiles over `claude` too, each
  through the profile's own `[env]` (`CLAUDE_CONFIG_DIR`), although such a
  profile does not resolve without the package. A bare `integration status` or
  `doctor` without `--agent` covers only served runtimes, so name `--agent
  claude` to see leftover Claude hooks.
- A host profile with `base = "claude"` and no `package`/`digest` keys fails
  with `runtime_not_installed`; its hint names the install and then `pohunek
  plugin profile migrate <name>`, which pins the profile to the installed
  package.
- A `claude` in the foreground of a shell session, or started outside pohunek,
  is not attributed to Claude Code: process recognition and transcript
  observation read the package's `detect.toml` and `[transcripts]` table.
- A session the built-in Claude of an earlier release launched is refused on
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
- A package that declares `claude-hook-v1` and Claude's `[config_home]` under
  another runtime id reaches the same `~/.claude`. Requests that name no runtime
  attribute that home to neither runtime (`integration_home_ambiguous_owner`)
  and a bare install writes nothing; name the runtime with `--agent`.
- A bare `pohunek integration install` on a host without the package fails with
  `integration_no_managed_runtime`; its hint names the package install and then
  `pohunek integration install --agent claude`.
- A shell session hosting `claude` stays interactive without the package:
  input, attach and resize act on the shell, so a Claude identity its hooks
  report does not need to resolve.

## What the package declares

The package is version `1.0.1`. Every descriptor fact below equals the
built-in Claude descriptor release 0.34.2 shipped (package version `1.0.0`),
except the version probe, the transcript tree (`[transcripts]`) and the input
readiness (`[input] ready`), which that descriptor does not declare. The
detection manifest is the v0.34.2 built-in one
plus the rules of #613 ([below](#detection-beyond-the-built-in-manifest)). The
package tests
compare the package with golden copies of that descriptor and its detection
manifest in `compat/claude/builtin-v0.34/` (`runtime.toml`, `detect.toml`):
the manifest may differ only by the enumerated #613 changes, and only the
enumerated captured frames may read differently.

- Launch: `claude` with the first prompt as a trailing positional argument.
  Claude's Ink input takes text without bracketed paste; the submit key is a
  separate Enter written 150 ms later, and a submit delay the daemon is
  configured with for this runtime replaces the 150 ms
  (`submit_delay_configurable = true`).
- Readiness: `ready = "screen_idle"`. Claude enables bracketed paste before its
  prompt box accepts a paste, and pastes sent until the box renders are dropped
  (probe: a paste 0, 200 or 500 ms after the `ESC[?2004h` enable was lost, a
  paste right after the first `❯` render was accepted in 6 of 6 runs). Only the
  rendered prompt box, the `live_prompt_box` rule of the detection manifest,
  starts the initial input. A session whose prompt box does not render within
  the host's `initial_startup_grace_ms` fails `session.new` with
  `session_input_not_ready` and is rolled back; the initial input is not
  written. Daemons of release 0.34.x reject a descriptor that carries `ready`.
  A host profile whose override manifest has no idle rule on a screen region
  (such as `live_prompt_box`) launches with bracketed-paste readiness instead,
  and `pohunek doctor` warns about it.
- Version: `version_probe = { parser = "semver-line-v1", args = ["--version"],
  min = "2.1.289", below = "2.2.0", line = "{version} (Claude Code)" }`.
  `claude --version` prints `2.1.289 (Claude Code)` on standard output; any
  other wording, a pre-release or trailing text is unsupported. The lower bound
  is the one release that was verified; the upper bound is the next minor
  release, as for the other packages. Claude Code ships patch releases almost
  daily, so only 2.1.289 was exercised; moving the range is a lock and
  descriptor change made together after a real-Claude run of the new release.
- Native reference: `strategy = "hook"`; resume is `claude --resume <id>`; fork
  is `claude --resume <id> --fork-session`.
- Integration: handler `claude-hook-v1` with hook schema
  `identity-subagent-v1`. The package names the compiled handler and schema; the
  hook reporter scripts stay core-owned, so the archive carries no asset.
- Config home: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
- Detection (`detect.toml`): process matchers are the kernel name `claude` or
  `claude.exe`, a command line whose first argument is `claude` or
  `claude.exe` (with or without a directory), or a command line whose first
  argument is `node` or `bun` followed by flag options (`-…`, no separate
  values) and a script named `claude` (also `.js`, `.mjs`, `.cjs`) or lying in
  the `@anthropic-ai/claude-code` package. A command that only names a Claude
  executable as a later argument (`ls -l …/claude.exe`, `tail -f /work/claude`)
  does not match. Rules read the
  screen: a status line above the prompt box (`✶ Billowing… (3s · ↓ 2 tokens)`)
  or the footer `esc to interrupt` below it means `working` and outranks the
  visible prompt box; a visible prompt box or a completed-turn line
  (`✻ Brewed for 7s · done 3:34 PM`) means `idle`; the tool approval dialog
  (`Do you want to proceed?` with its options), the selection forms
  (`Enter to select · ↑/↓ to navigate · Esc to cancel`, also wrapped), the
  first-run theme picker, API-key approval and login method list, and the
  folder trust dialog mean `blocked`. A title that starts with `◐` or `◑` (or a
  braille spinner) means `working`, a title that starts with `✳` means `idle`.

## First-run state of a Claude home

A fresh `CLAUDE_CONFIG_DIR` shows first-run screens. The state lives in two
files inside that directory, which a test seeds deliberately and which the
capture also records from a fresh home:

- `settings.json`: `{"theme": "dark"}` ends the theme picker.
- `.claude.json`: `hasCompletedOnboarding: true` ends the welcome flow;
  `customApiKeyResponses.approved` lists the last 20 characters of an API key
  the owner approved (the key in `ANTHROPIC_API_KEY` otherwise prompts `Detected
  a custom API key`, whose default answer `No` leads to the login screen);
  `projects.<cwd>.hasTrustDialogAccepted: true` ends the folder trust dialog.

A fresh home makes Claude's startup preflight send an unauthenticated `HEAD
/api/hello` to the configured `ANTHROPIC_BASE_URL` and to `api.anthropic.com`,
and Claude exits with `Unable to connect to Anthropic services` when
`api.anthropic.com` is unreachable (observed in a network namespace with only
loopback). With a seeded home Claude still sends the `HEAD /api/hello` to the
base URL (the stub answers 404) but keeps running without network access, so
every test except the first-run capture runs without egress.

## Verified against a real Claude Code 2.1.289

The screens in `compat/claude/screens/` (named screens at 100 columns, widths 20
to 200 in `widths/`, each with the OSC title Claude had set in a `.title` file)
were captured from a real interactive Claude Code in a PTY driven by a real
daemon (`capture_the_real_claude_first_run_screens` and
`capture_the_real_claude_screens` in `crates/cli/tests/claude_package.rs`) with a
throwaway `HOME` and `CLAUDE_CONFIG_DIR`, a dummy API key
(`pohunek-stub-key-…`), a loopback Messages-API stub as the model
(`crates/cli/tests/support/messages_stub.rs`) and no real credential. The first-run
capture needs egress for the preflight above; the seeded captures ran in a
network namespace with only loopback. They cover the theme picker, the API-key
approval, the login method list, the folder trust dialog, the idle screen, a turn
held before its first output, a streaming reply, the screen after a turn, a Bash
tool approval and a multiple-choice question. The binary was the linux-x64
native executable of 2.1.289 from the distribution package, byte-identical
(SHA-256 in the lock) to the one the npm package installs; the real-Claude tests
also ran against the npm install. The darwin-arm64 executable of the same release is pinned by its own SHA-256 and runs in the aarch64-apple-darwin release-evidence row; the screen captures were not repeated on macOS. `crates/cli/tests/claude_package.rs` classifies every
captured screen through the package manifest.

- The idle, working and blocked states are read from the screen and the title.
  Claude keeps its idle title (`✳ Claude Code`) while an approval or question
  dialog is open, and the screen rules (`blocked`, priority 840 and above) win
  over it. While a turn runs the title alternates `◐ Claude Code` and
  `◑ Claude Code` every 960 ms (the title component of the 2.1.289 binary).
- Claude runs a hook command as `/bin/sh -c <command>`. Where `/bin/sh` is dash
  (Debian, Ubuntu, so the hosted CI runner) the shell stays between Claude and
  the reporter script, so the script's `$PPID` is that shell and the worker
  rejected the SessionStart report: no native reference, no resume. The managed
  Claude scripts now propose the parent of a wrapper shell as the reporting
  process (the `agent-pid` block, which reads the untruncated command line with `ps -ww` and compares the script path quoted as the installer quotes it, so long paths and paths with an apostrophe match; tests in `crates/daemon/src/integration/hook_pid_tests.rs`),
  and the real-Claude tests were also run with dash as `/bin/sh`. The Codex
  scripts keep `$PPID`.
- The hook runs from the launched process: the SessionStart report names the
  launch process as the reporter, so the conversation id becomes the session's
  native reference and names Claude's transcript file
  (`projects/<folder>/<id>.jsonl`). Unlike Codex there is no nested reporter.
- Resume starts `claude --resume <id>` and fork starts `claude --resume <id>
  --fork-session`; both were checked on the process command line of a real run.
- A fork is a new conversation: Claude fires SessionStart for it (`source:
  fork`) with the fork's own conversation id. The forked session holds no
  reference until its launch process reports one, so that id becomes the fork's
  native reference while the source keeps its own, and resuming the fork starts
  `claude --resume <fork id>` without `--fork-session`
  (`a_real_claude_resumes_and_forks_with_the_descriptor_arguments`).
- A subagent the model starts through the `Task` tool is reported by the
  SubagentStart and SubagentStop hooks (`identity-subagent-v1`) as a Claude
  subagent that runs and then completes.
- Claude Code 2.1.295 fires SubagentStart again with the same `agent_id` when a
  finished background agent is resumed with `SendMessage` or wakes itself after
  its own background shell ends, and a SubagentStop at the end of that run; the
  worker reopens the terminal record as `running` and completes it again.
  About 1 to 3 seconds after every main-agent Stop, Claude fires a
  SubagentStop with a fresh `agent_id`, an empty `agent_type` and no preceding
  start for its prompt-suggestion helper; `identity-subagent-v1` admits a
  Claude stop only after a recorded start, so the helper adds no subagent row
  (evidence in [#883](https://github.com/zajca/pohunek/issues/883)).
- The main-agent Stop input carries `background_tasks`, the backgrounded tasks
  still running or pending (Claude Code 2.1.296). A background subagent's task
  id is its `agent_id`; teammates, synchronous children and workflow agents are
  not listed by agent id. The installer registers two Stop hooks in
  `settings.json`: the notify hook (`turn_completed`) and the state hook
  (`stop`), which sends each task's `id`, `type` and `status` as a
  `subagent_snapshot` claim to the worker, with three optional fields sanitized
  in the hook: `label` (from `description`, for every type; control and format
  characters removed, whitespace collapsed, cut to the configured byte limit),
  `program` (the basename of the command word, for `shell` tasks only; `NAME=value`
  assignments, `cd <dir> &&` and `env` options are skipped, and a name that does
  not match `[A-Za-z0-9][A-Za-z0-9._+-]*` is omitted) and `agent_type` (for
  `subagent` tasks only; a value containing a control, format, line or paragraph
  separator or surrogate character is dropped from its task). A command longer
  than 64 KiB gets no `program`, because shell tokenizing grows superlinearly
  with token length (about 32 ms at 64 KiB, 6 s at 1 MiB) and a cut command word could name another program. The command and its
  arguments are never forwarded.
  The label and program limits come from `POHUNEK_BACKGROUND_LABEL_MAX_BYTES` and
  `POHUNEK_BACKGROUND_PROGRAM_MAX_BYTES`, which the worker injects; a missing or
  invalid value forwards no label or program. An invalid optional field is
  dropped from its task only, while a missing or invalid `id`, `type` or
  `status` voids the list. The hook reads at most 1 MiB and sends nothing for a
  Stop with a non-empty `agent_id` or a missing, invalid or over-long list. Claude
  builds the list before the hook reads its clock and
  often still lists a fast background agent whose own SubagentStop already
  arrived, so the worker never reopens a listed row: it completes background
  agents of the same Claude process that an earlier list named and this one
  omits, and leaves listed rows to their own start and stop (see
  [sessions](../concepts/sessions.md); evidence in
  [#883](https://github.com/zajca/pohunek/issues/883)).
- The `session` action (Claude `SessionStart`) forwards the hook input `source`
  as `session_source` in its identity report only when it is `startup`,
  `resume`, `clear` or `compact`; any other value, or none, omits the key.
  `fork` is not forwarded: a forked session is a new process and must not clear
  the snapshot of the session it was forked from. A `SessionStart` from another
  process clears the snapshot; `startup` and `resume` of the same process clear
  it too, while `clear` and `compact` keep it.
- The `release` action (Claude `SessionEnd`) forwards the hook input `reason` as
  `release_reason` in its `identity_release` request only when it is `clear`,
  `resume`, `logout`, `prompt_input_exit` or `other`; any other value omits the
  key. The snapshot belongs to the reporting Claude process: any process in the
  managed PTY tree may report, the latest turn end wins, and the owner's release
  with a reason other than `clear` clears it. A `/clear` keeps it, because Claude
  follows that release with a `SessionStart` of the same process.
- A Claude state hook action (`session`, `release`, `subagent-start`,
  `subagent-stop`, `stop`) that fails to deliver its report, including a `stop`
  that sends no snapshot, still exits 0 and appends one payload-free line per
  reason (`provider` `claude`, the action and a reason code) to the hook failure trace
  `pohunek-hooks.jsonl` in the pohunek log directory (see the
  [debug-session-runtime](../runbooks/debug-session-runtime.md) runbook).
- `claude --version` prints `2.1.289 (Claude Code)`; the inventory shows the
  package's probe accepted the real release.
- Environment that keeps a real Claude away from host state and services:
  `CLAUDE_CONFIG_DIR`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY` (dummy),
  `ANTHROPIC_MODEL`, `DISABLE_AUTOUPDATER`, `DISABLE_UPDATES`,
  `DISABLE_TELEMETRY`, `DISABLE_ERROR_REPORTING`,
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` and
  `CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL` (without the last one a
  first interactive launch registers the official plugin marketplace).

## Detection beyond the built-in manifest

The package manifest adds these rules to the v0.34.2 built-in one (#613). Each is
backed by screens or process forms captured from 2.1.289 and by a package test
that fails without the rule.

- **Running-turn title.** `osc_title_working` also takes `◐` and `◑`, the
  glyphs 2.1.289 alternates before its title while a turn runs
  (`the_title_reads_idle_for_the_asterisk_and_working_for_the_turn_glyphs`).
  The title outranks the screen whenever it changes.
- **Running-turn footer.** A streaming reply hides the status line, and the
  daemon evaluates the title only when it changes, so a reply re-rendered
  between two title changes would read idle through the visible prompt box.
  The footer below the prompt box reads `esc to interrupt` while a turn runs
  (`? for shortcuts` while Claude waits); `live_turn_footer` reads it as
  `working` at the priority and with the dialog exclusions of the status-line
  rule (`a_streaming_reply_reads_working_from_its_title_and_turn_footer`,
  `a_real_claude_reply_held_mid_stream_reads_working`).
- **Wrapped selection forms.** `live_blocked_form` separates the words of its
  hints by any whitespace, so the question form reads `blocked` at 30 and 40
  columns, where its footer wraps (`Enter to select · ↑/↓ to` /
  `navigate · Esc to cancel`).
- **First-run and trust dialogs.** `theme_picker` (`Choose the text style…`
  with the dark and light modes), `login_method_list` (`Select login method`
  with the subscription and Console options) and `folder_trust_dialog`
  (`Yes, I trust this folder`, `No, exit`, `Enter to confirm`) read `blocked`;
  the trust dialog at every captured width, its heading scrolled away at 20
  columns. These are the dialogs Claude waits on before it opens its prompt.
  The rules read only the text below the last horizontal rule on screen, where
  Claude draws these dialogs; a transcript that quotes them sits above the
  prompt box's rules, so the screen keeps reading idle or working
  (`quoted_dialog_text_in_the_transcript_is_not_a_dialog`).
- **Completed-turn line.** `completed_turn_status` takes the ` · done <time>`
  2.1.289 appends (`✻ Brewed for 7s · done 3:34 PM`; the clock follows the host
  locale).
- **Process forms.** The npm package's executable `bin/claude.exe` runs as kernel
  name `claude.exe` with a command line ending in `claude.exe`; both matchers
  accept it, and the real launch test requires the launched process to match
  with either install. Through `node` or `bun` the script must be named
  `claude` or lie in the `@anthropic-ai/claude-code` package, so a script that
  merely carries the word (`node /opt/claude-companion.mjs`) is not Claude,
  and a node option with a separate value (`node -r ./preload.js …/claude`)
  hides the script that follows it.
  A launcher that only names the package (`npx @anthropic-ai/claude-code`,
  `bun x @anthropic-ai/claude-code`) is not matched itself; the `claude.exe` it
  starts is (`the_process_matchers_accept_the_observed_claude_process_forms`).

Limits that remain:

- At 30 columns and fewer the footer is cut before `esc to interrupt` (`⏸ manual
  mode on · esc to…`), so a streaming reply re-rendered there reads idle until
  the next title change.
- Under a terminal multiplexer (`TMUX`, `STY` or `ZELLIJ` in Claude's
  environment) 2.1.289 keeps the static `✳` title during a turn (its
  `tengu_static_title_under_mux` switch, on by default); the screen rules are
  then the only working evidence. Not exercised in a real run.
- At 20 and 30 columns the completed-turn line wraps before ` · done`, so the
  prompt box alone reads idle there.
- The theme picker and the login method list were captured at 100 columns
  only; the rules separate their words by any whitespace for narrower
  terminals, which no capture confirms.

## Not verified

Releases other than 2.1.289 (the range above the lock is a policy, not a
verification), the native installer's `~/.local/bin/claude` launcher and the
Homebrew and apt installs (the npm install and the distribution package ship the
same binary), macOS and Windows, a real login or a real model, plan mode,
compaction, MCP and other dialogs, the `PermissionRequest` hook, terminals
narrower than 20 columns, and a hosted CI run of the `claude-package` job. The
first-run capture needs egress; the absence of egress in every other run is
established by the network namespace, not by Claude's own switches.

## How the package is kept honest

- `crates/cli/tests/claude_package.rs` always runs: it builds the directory
  twice and requires identical bytes, parses it with the daemon's install
  parser, compares every descriptor fact and the whole manifest with the
  v0.34.2 built-in Claude goldens in `compat/claude/builtin-v0.34/` (as parsed
  definitions, as TOML data, and by feeding both manifests every captured frame,
  title and synthetic dialog; the manifest differences and the frames they
  reclassify are enumerated, each with what the package reads), checks resume and
  fork argv, requires the supported range to equal
  `compat/claude/compatibility-lock.json`, reads the version banner, runs the
  title, screen and process rules, and asserts the `--sha256` install refusal
  above.
- The daemon-backed tests install the built archive through `pohunek plugin
  install --catalog`, the way a host with a catalog trust anchor does: the test
  process generates a throwaway signing key, gives the daemon an anchor that
  trusts it (`crates/test-support/src/catalog.rs`, feature `catalog`), and
  signs a catalog entry binding the package id, the runtime id `claude` and the
  archive digest.
  The key never leaves the process and no real key or catalog is involved. The
  package then serves `claude` with official trust, profiles carry its
  `package`/`digest` pin, and a launch runs the package's version probe on the
  executable the profile names. An always-running test drives that probe with
  throwaway `claude` scripts: the older release, the next minor, a pre-release,
  a different banner wording and unreadable output are refused with
  `agent_runtime_unsupported`, and the supported banner launches.
- Always-running clean-host tests start from a daemon without the package and
  a fake `claude` of the locked release on `PATH`: the bare runtime, its
  integration and an unpinned `base = "claude"` profile are refused with the
  install hint (and the migration for the profile), and after the catalog
  install the bare runtime launches pinned to the package and the migrated
  profile launches. Others install the hooks into the hermetic `~/.claude` (and
  into two host profiles' own `CLAUDE_CONFIG_DIR`) through the package,
  uninstall the package, and check that `integration status`, `doctor` and
  `uninstall --agent claude` (with `--profile` and `--all-profiles`) still
  report and remove them while `integration install` is refused, that a
  malformed profile is refused instead of being read with an empty
  environment, and that the assistant answers `no_capable_agent` on such a
  host. One more installs the hooks and checks that the installed reporter
  scripts equal the recorded renderings
  (`crates/daemon/src/integration/reporter_golden/claude-*.sh`) byte for byte.
- The test daemon observes the host through the test-host process view
  (`crates/daemon/src/procwatch/readable_host.rs`, via
  `SessionRegistry::new_production_with_inspector`), so a same-user process of
  the test host that started after a worker and hides its environment (another
  parallel test's fresh worker, a non-dumpable helper) does not make a session
  removal refuse with `runtime_supervision_ambiguous`. The hosted runner hit
  that refusal once in `a_real_claude_resumes_and_forks_with_the_descriptor_arguments`;
  `a_session_removal_ignores_an_unreadable_process_of_the_test_host` reproduces
  it with a non-dumpable helper. The production daemon keeps the fail-closed
  host view.
- The fixture owns a process guard (`crates/cli/tests/support/process_guard.rs`):
  on success and on unwind it kills every process whose executable, working
  directory or environment value lies below the fixture's root before the
  directories are removed. Two tests pin the guard.
- `#[ignore]`d tests drive a real `claude` through the installed package with a
  throwaway Claude home and the loopback Messages stub: the banner against the
  probe template, launch and detection (input framing, the held turn, the
  process matchers on the real process; the stub sends no text until its start
  gate opens, so the working screen is stable), a reply held mid-stream (it
  must not read idle for longer than two title changes), the SessionStart
  reference and its transcript,
  resume and fork argv, the subagent hooks (the same subagent must reach
  `completed` while the parent runtime is live), and the approval dialog. The
  subagent tracking of #883 has three more:
  `a_resumed_real_claude_background_agent_keeps_one_row_through_both_runs`
  (a background agent resumed with `SendMessage` keeps one row, `running`
  during each run and `completed` after it),
  `a_real_claude_background_agent_without_its_stop_is_settled_by_the_main_stop`
  (pohunek's SubagentStop hook is removed; an earlier main Stop listed the
  running agent, and the first main Stop whose list omits the finished agent
  completes it, logged as `snapshot_absent`) and
  `real_claude_prompt_suggestion_stops_create_no_subagent_row` (the
  prompt-suggestion helper's stops create no row). The helper test sets
  `CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION=1` in the session profile, because
  without it Claude reads the helper's enablement from a remote feature flag
  that a hermetic run never fetches.
  Hooks come from `pohunek integration install --agent claude --profile …`,
  which resolves the profile's `CLAUDE_CONFIG_DIR` so the real home is never
  touched. Run them with `POHUNEK_CLAUDE_E2E=1 cargo test -p pohunek-cli --test
  claude_package -- --include-ignored --test-threads 1`; without the variable and
  a `claude` on `PATH` they fail by name instead of skipping. Never point a
  manual run at `~/.claude`, and do not run it from inside a Claude Code session
  without removing every `ANTHROPIC_*` and `CLAUDE*` variable: the fixture gives
  Claude its own environment, but the surrounding shell must not carry a real
  key into the test process.
- To prove the absence of egress, run the test binary in a network namespace
  with only loopback (`unshare -rn --pid --fork --mount-proc`, then `ip link set
  lo up`). The daemon trusts an executable only when root or the effective user
  owns it, so under the namespace's user mapping put a user-owned copy of the
  `claude` binary first on `PATH`; the pid namespace keeps host processes out of
  the daemon's runtime-release scan.
- Refreshing the screens: `POHUNEK_CLAUDE_E2E=1 POHUNEK_CLAUDE_CAPTURE_DIR=<abs
  dir> cargo test -p pohunek-cli --test claude_package -- --ignored
  capture_the_real_claude` records the first-run screens (needs egress) and the
  seeded screens (run it without egress); without the directory variable the
  capture tests do nothing.
- The `claude-package` CI job installs exactly the locked release from npm
  (`@anthropic-ai/claude-code@2.1.289`, with install scripts enabled because the
  postinstall links the native binary; `--ignore-scripts` leaves a launcher that
  refuses to run), compares the native binary with the lock's SHA-256, builds the
  archive with `cargo xtask package build` and runs that file.

Update procedure for a new Claude Code release: install it, rerun the
real-Claude tests, recapture the screens if the interface changed, move the lock
(release, SHA-256) and the descriptor's `min`/`below` together, and bump the
package `version`.
