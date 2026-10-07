---
type: Guide
id: guide/claude-package
title: Claude Code runtime package
description: The official Claude Code runtime package source (Claude Code 2.1.x from 2.1.289), what it declares and keeps equal to the built-in Claude runtime, what was verified against a real Claude Code, and the gaps found.
source_kind: manual
intents: [setup, update, debug, help]
---

# Claude Code runtime package

The Claude package (`runtime-packages/claude`: `runtime.toml` and `detect.toml`,
nothing else) is the package form of the Claude Code runtime that is compiled
into the daemon today. Its compatibility lock and the screens captured from a
real Claude Code are in `compat/claude/`. See [runtime
packages](runtime-packages.md) for the generic commands and the [Codex
package](codex-package.md) for the package this one mirrors.

## Status: official source, no release catalog yet

`claude` is a reserved runtime id. A package serves it only with official trust:
a `--catalog` install whose signed catalog entry binds the package id, runtime
id and digest, verified against the host's catalog trust anchor (see [runtime
packages](runtime-packages.md)). An install trusted by `--sha256` is refused
with `package_runtime_not_claimable`. The repository ships no signing key and no
release catalog, so no real host can authorize the package yet; the built-in
descriptor, detection manifest and Claude handler keep serving `claude`, and
removing them waits for the production catalog key. The package tests keep the
package equal to the built-in until the built-in is removed.

## What the package declares

Every fact below equals the built-in Claude descriptor
(`crates/daemon/src/agent/builtin/claude.toml`), except the version probe, which
only the package declares.

- Launch: `claude` with the first prompt as a trailing positional argument.
  Claude's Ink input takes text without bracketed paste; the submit key is a
  separate Enter written 150 ms later, and a submit delay the daemon is
  configured with for this runtime replaces the 150 ms
  (`submit_delay_configurable = true`).
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
- Detection (`detect.toml`, the built-in manifest as a file): process matchers
  are the kernel name `claude` or a command line whose program is `claude`
  (also through `node` or `bun`). Rules read the screen: a status line above the
  prompt box (`✶ Billowing… (3s · ↓ 2 tokens)`) means `working` and outranks the
  visible prompt box; a visible prompt box means `idle`; the tool approval
  dialog (`Do you want to proceed?` with its options), the selection forms
  (`Enter to select · ↑/↓ to navigate · Esc to cancel`) and the first-run API-key
  approval mean `blocked`; a title that starts with a braille spinner means
  `working`, a title that starts with `✳` means `idle`.

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
also ran against the npm install. `crates/cli/tests/claude_package.rs` classifies every
captured screen through the package manifest.

- The idle, working and blocked states are read from the screen. Claude keeps
  its idle title (`✳ Claude Code`) while an approval or question dialog is open,
  and the screen rules (`blocked`, priority 840 and above) win over it.
- The hook runs from the launched process: the SessionStart report names the
  launch process as the reporter, so the conversation id becomes the session's
  native reference and names Claude's transcript file
  (`projects/<folder>/<id>.jsonl`). Unlike Codex there is no nested reporter.
- Resume starts `claude --resume <id>` and fork starts `claude --resume <id>
  --fork-session`; both were checked on the process command line of a real run.
- A subagent the model starts through the `Task` tool is reported by the
  SubagentStart and SubagentStop hooks (`identity-subagent-v1`) as a Claude
  subagent that runs and then completes.
- `claude --version` prints `2.1.289 (Claude Code)`; the inventory shows the
  package's probe accepted the real release.
- Environment that keeps a real Claude away from host state and services:
  `CLAUDE_CONFIG_DIR`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY` (dummy),
  `ANTHROPIC_MODEL`, `DISABLE_AUTOUPDATER`, `DISABLE_UPDATES`,
  `DISABLE_TELEMETRY`, `DISABLE_ERROR_REPORTING`,
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` and
  `CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL` (without the last one a
  first interactive launch registers the official plugin marketplace).

## Gaps found

- **The working title is not a rule.** Claude 2.1.289 sets a rotating
  circle-quadrant glyph (`◐ Claude Code`, `◑ …`) while a turn runs. The
  manifest's `osc_title_working` names braille spinners, so the title speaks for
  idle only. A running turn is working because the status line is on screen; a
  streaming reply without a status line (the captured `working_stream`) reads
  idle through the visible prompt box although the title says a turn is running.
  Tests pin both (`the_idle_title_is_a_rule_and_the_working_circle_title_is_not`,
  `a_streaming_reply_without_a_status_line_reads_idle_from_the_screen`). A title
  rule for the circle glyphs is a change to both manifests, decided by the owner.
- **Narrow question forms.** At 30 and 40 columns the footer of the question form
  wraps between its hints, so no rule matches and the screen reads idle; at 20
  columns every hint has its own line and the form is blocked again.
- **First-run and trust dialogs.** The API-key approval is blocked; the theme
  picker, the login method list and the folder trust dialog (`Quick safety check:
  Is this a project you created or one you trust?`) match no rule, so those
  sessions keep the byte-activity fallback.
- **The completed-turn line carries a suffix.** Claude 2.1.289 prints
  `✻ Brewed for 7s · done 3:34 PM`; `completed_turn_status` expects the line to
  end after the seconds. The visible prompt box still reads idle.
- **A fork keeps its source's reference.** Claude fires SessionStart for the
  fork (`source: fork`) with the fork's own conversation id, but the forked
  session already holds the reference it inherited from its source, and the
  daemon rejects the worker's identity snapshot as
  `launch_identity_reference_mismatch` (`crates/daemon/src/session/reconcile.rs`).
  The fork therefore records no conversation id of its own, and resuming it later
  would resume the source conversation. The built-in Claude runtime behaves the
  same. A real-Claude test pins this
  (`a_real_claude_resumes_and_forks_with_the_descriptor_arguments`) and fails when
  the daemon adopts the fork's id.
- **The npm install is not matched by process.** The npm package's executable
  is `bin/claude.exe`; a session launched from it runs as kernel name
  `claude.exe` with a command line ending in `claude.exe`, and neither process
  pattern matches it (`^claude$`, `(^|/)claude($| )`). The distribution package
  names the binary `claude` and matches (the native installer was not run).
  Launch, detection from the screen and the hook-reported reference behave the
  same with both, and the real-Claude tests passed with both installs; a test pins the forms (`the_process_matchers_accept_the_observed_claude_process_forms`
  and the real launch test). Accepting `claude.exe` is a change to both
  manifests, decided by the owner.
- **Process matcher looseness.** The command-line pattern `node|bun … \bclaude\b`
  also matches a script whose name contains `claude` as a word
  (`node /opt/claude-companion.mjs`); a test pins it.

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
  built-in Claude files (as parsed definitions, as TOML data, and by feeding both
  manifests every captured frame, title and synthetic dialog), checks resume and
  fork argv, requires the supported range to equal
  `compat/claude/compatibility-lock.json`, reads the version banner, runs the
  title, screen and process rules, and asserts the `--sha256` install refusal
  above.
- The daemon-backed tests install the built archive through `pohunek plugin
  install --catalog`, the way a host with a catalog trust anchor does: the test
  process generates a throwaway signing key, gives the daemon an anchor that
  trusts it (`crates/cli/tests/support/catalog_fixture.rs`), and signs a catalog
  entry binding the package id, the runtime id `claude` and the archive digest.
  The key never leaves the process and no real key or catalog is involved. The
  package then serves `claude` with official trust, profiles carry its
  `package`/`digest` pin, and a launch runs the package's version probe on the
  executable the profile names. An always-running test drives that probe with
  throwaway `claude` scripts: the older release, the next minor, a pre-release,
  a different banner wording and unreadable output are refused with
  `agent_runtime_unsupported`, and the supported banner launches.
- The fixture owns a process guard (`crates/cli/tests/support/process_guard.rs`):
  on success and on unwind it kills every process whose executable, working
  directory or environment value lies below the fixture's root before the
  directories are removed. Two tests pin the guard.
- `#[ignore]`d tests drive a real `claude` through the installed package with a
  throwaway Claude home and the loopback Messages stub: the banner against the
  probe template, launch and detection (input framing, the held turn, the
  process matchers on the real process), the SessionStart reference and its
  transcript, resume and fork argv, the subagent hooks, and the approval dialog.
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
