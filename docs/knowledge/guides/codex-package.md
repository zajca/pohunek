---
type: Guide
id: guide/codex-package
title: Codex runtime package
description: The official Codex runtime package source (Codex 0.160.x), what it declares and keeps equal to the built-in Codex runtime, what was verified against a real Codex, and the gaps found.
source_kind: manual
intents: [setup, update, debug, help]
---

# Codex runtime package

The Codex package (`runtime-packages/codex`: `runtime.toml` and `detect.toml`,
nothing else) is the package form of the Codex runtime that is compiled into the
daemon today. Its compatibility lock and the screens captured from a real Codex
are in `compat/codex/`. See [runtime packages](runtime-packages.md) for the
generic commands and [Pi](pi-package.md) for the package this one mirrors.

## Status: official source, no release catalog yet

`codex` is a reserved runtime id. A package serves it only with official trust:
a `--catalog` install whose signed catalog entry binds the package id, runtime
id and digest, verified against the host's catalog trust anchor (see [runtime
packages](runtime-packages.md)). An install trusted by `--sha256` is refused
with `package_runtime_not_claimable`. The repository ships no signing key and no
release catalog, so no real host can authorize the package yet; the built-in
descriptor, detection manifest and Codex handler keep serving `codex`. The
package tests keep the package byte-for-fact equal to the built-in until the
built-in is removed.

## What the package declares

Every fact below equals the built-in Codex descriptor
(`crates/daemon/src/agent/builtin/codex.toml`), except the version probe, which
only the package declares.

- Launch: `codex` with the first prompt as a trailing positional argument;
  bracketed paste followed by a separate Enter 150 ms later for later input.
- Version: `version_probe = { parser = "semver-line-v1", args = ["--version"],
  min = "0.160.0", below = "0.161.0", line = "codex-cli {version}" }`. `codex
  --version` prints `codex-cli 0.160.0` on standard output; the probe reads only
  that first line, so the `WARNING: proceeding, even though we could not create
  PATH aliases` line Codex writes to standard error when its home is under a
  temporary directory is ignored. A pre-release (`codex-cli 0.161.0-rc.1`) or
  any other wording is unsupported. The range is one minor release because
  Codex is `0.x`: a new minor can change the interface.
- Native reference: `strategy = "hook"`; resume is `codex resume <id>`; fork is
  unsupported.
- Integration: handler `codex-hook-v1` with hook schema `identity-subagent-v1`.
  The package names the compiled handler and schema; the hook reporter scripts
  stay core-owned, so the archive carries no asset.
- Config home: `CODEX_HOME`, else `~/.codex`.
- Detection (`detect.toml`): process matchers are the kernel name `codex` or a
  command line whose program is `codex`. Codex reports its state through the
  terminal title, so the idle, working and blocked rules read the title:
  a title that starts with a braille spinner means `working`; a title that
  contains `Action Required` (Codex writes `[ ! ] Action Required | <dir>`,
  alternating `[ . ]`) means `blocked` and outranks `working`; any other
  non-blank title means `idle`, read from the absence of a spinner and of
  `Action Required` rather than from a title of its own. Screen rules cover
  prompts that block the turn: the first-run workspace trust dialog at the top
  of the screen, the confirmation lines Codex prints after its last prompt
  marker (`press enter to confirm or esc to cancel`, `enter to submit answer`,
  `enter to submit all`, `allow command?`) and the weaker `[y/n]`-style prompts
  anywhere in the recent screen.

## Verified against a real Codex 0.160.0

The screens in `compat/codex/screens/` (named screens at 100 columns, widths 20
to 200 in `widths/`, each with the OSC title Codex had set in a `.title` file)
were captured from a real interactive Codex in a PTY with a fresh `CODEX_HOME`,
a loopback Responses stub as the model provider, no credential, and no network
(a network namespace with only loopback). They cover the idle screen, a turn
held before its first output (`Working (2s • esc to interrupt)`), a streaming
reply, the screen after a turn, a tool approval prompt (`Would you like to run
the following command?`), a question prompt (`request_user_input`), the
first-run folder dialog, and the sign-in screen. `crates/cli/tests/codex_package.rs`
classifies every one of them through the package manifest.

- Title rules: idle, working and blocked are read from the title at every
  captured width; the screen text has no working or idle rule.
- Screen rules: the approval and question prompts are blocked from the screen
  text alone at 30 columns and wider. At 20 columns the confirmation line wraps
  and only the title says blocked.
- `codex resume <id>` against a fresh home restores the conversation from
  Codex's own session store (checked by hand; the id is the one in
  `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`).
- Hook trust: the records pohunek's integration installer writes
  (`[hooks.state."<hooks.json>:<event>:0:0"] trusted_hash`) are accepted, and the
  SessionStart hook reports the conversation id to the daemon.

## Gaps found

- **The folder dialog is not classified.** Codex 0.160.0 shows `Trust this
  folder? Codex can read, edit, and run files here…` with `1. Trust and
  continue` and `2. Back to Agent Command Center`; it appears when the working
  directory carries a project-local `.codex/config.toml`. The
  `workspace_trust_prompt` rule matches the older wording (`Do you trust the
  contents of this directory?`, `1. Yes, continue`), so the real dialog is read
  by no rule and the session keeps the byte-activity fallback. The package keeps
  the built-in rule unchanged and a test pins the non-match
  (`the_real_trust_folder_dialog_is_not_yet_a_blocked_screen`). A rule for the
  new wording is a change to both manifests, decided by the owner.
- **Hooks run in a child process.** Codex 0.160.0 runs its hooks from a `codex
  app-server --listen unix:// --managed-daemon` process that is a child of the
  launched `codex`. The daemon accepts native-reference reports only from the
  launch process, so the conversation id arrives as the nested active agent's
  (`active_agent_session_id`) and `native_session_id` stays empty: `session
  resume` is refused with `not_resumable`. A real-Codex test pins this
  (`a_real_codex_reports_hooks_from_its_app_server_child_so_resume_has_no_reference`).
  The app-server also outlives the TUI when it is detached; the tests stop every
  process whose executable lives under the fixture's Codex home.
- **Process forms.** The helper process has kernel name `codex` too, so the
  matchers accept it. A `node …/codex.js` command line (the npm launcher) is not
  matched by the command-line pattern; the process forms of an npm install were
  not observed.

## Not verified

The compaction and retry screens, the `PermissionRequest`, `Stop` and subagent
hooks, a Codex that starts in the alternate screen, terminals narrower than 20
columns, macOS and Windows, a real-network run (the real-Codex tests switch off
update checks, analytics, plugin sync and connectors but do not prove the
absence of egress), and the older folder-dialog wording the rule still names.

## How the package is kept honest

- `crates/cli/tests/codex_package.rs` always runs: it builds the directory
  twice and requires identical bytes, parses it with the daemon's install
  parser, compares every descriptor fact and the whole manifest with the
  built-in Codex files (as parsed definitions, as TOML data, and by feeding both
  manifests every captured frame, title and synthetic dialog), requires the
  supported range to equal `compat/codex/compatibility-lock.json`, reads the
  version banner, runs the title, screen and process rules, and asserts the
  install refusal above.
- Three `#[ignore]`d tests drive a real `codex` with a fresh `CODEX_HOME` and the
  loopback Responses stub (`crates/cli/tests/support/responses_stub.rs`): the
  banner against the probe template, launch and detection through the daemon
  (read through the package manifest as well), and the approval prompt. They
  launch through the built-in descriptor, which the parity tests prove equal to
  the package, because the package cannot be installed yet. The hook trust comes
  from `pohunek integration install --agent codex --profile …`, which resolves
  the profile's `CODEX_HOME` so the real home is never touched. Run them with
  `POHUNEK_CODEX_E2E=1 cargo test -p pohunek-cli --test codex_package --
  --include-ignored --test-threads 1`; without the variable and a `codex` on
  `PATH` they fail by name instead of skipping.
- The `codex-package` CI job installs exactly the locked release from npm
  (`@openai/codex@0.160.0`), builds the archive with `cargo xtask package build`
  and runs that file.

Update procedure for a new Codex release: install it, rerun the real-Codex
tests, recapture the screens if the interface changed, move the lock and the
descriptor's `min`/`below` together, and bump the package `version`.
