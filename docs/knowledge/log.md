# Knowledge Bundle Log

Each change records its entry as its own fragment under `log.d/`, named
`<issue>-<slug>.md`. The shipped bundle shows the fragments here as
`## Unreleased` sections, and `scripts/release` files them under the release
heading. Entries are newest first.

## 0.35.1

### 2026-10-16, macOS archive smoke warms python3 before the consumers, #1192

- On macOS, `packaging/smoke-archive` starts `python3` as the throwaway user,
  in the sandbox and with the consumer's `PATH`, before each consumer runs,
  prints how long it took and fails the smoke when it cannot start. A fresh macOS user's first start of the
  `/usr/bin/python3` shim took 17 s on macos-15, longer than the agents' 10 s
  hook timeout. Claude killed its first `SessionStart` hook before that hook
  reported the identity, so the session never got a native reference, and the
  v0.35.0 release smoke failed twice. Later starts take about 0.1 s.
- When a release consumer fails, the dump prints every hook line of each agent
  transcript, so a hook the agent killed is visible even outside the log tail.

## 0.35.0

### 2026-10-16, release evidence names the archive smoke matrix legs, #929

- `scripts/ci-evidence.json` requires both legs of the archive smoke matrix,
  `Archive smoke (network-isolated, ubuntu-latest)` and
  `Archive smoke (network-isolated, macos-15)`, in the main and nightly tiers.
  With the single pre-matrix name, `scripts/release` refused every release
  commit with `missing required job` after pushing it.
- `scripts/tests/test_ci_evidence` checks every required job name against the
  job names Actions can show. It covers the called job of a local reusable
  workflow (the part after ` / `) as well as the caller. A literal matrix
  expands to its exact names: each `${{ matrix.<key> }}` takes the
  combination's value, and a name without an expression gets the values
  appended in parentheses. Only a `fromJSON` matrix leaves an expression open.
  A required name that a matrix rename left behind, or a leg the workflow does
  not run, fails this test.

### 2026-10-15, SDK test request timeouts, #494

- The TS SDK relay suite (`sdk/ts/sdk/test/ws-transport.test.ts`) gives its
  clients a 2 s request timeout, `RELAY_REQUEST_TIMEOUT_MS`, instead of 20 ms.
  A round trip through the in-process relay exceeded 20 ms on a loaded macOS
  runner. Only the scenario that asserts the timeout passes a tiny value.
- The dedicated-routing scenarios in `request-response.test.ts` use a 1 s
  shared timeout, and daemon replies delayed 1.5 s. The dedicated deadlines
  are 4 s for the generic `session.wait` and `session.output` calls and 9 s
  for the input wait (its default 8 s plus 1 s headroom). The direct call the
  scenarios also make no longer has to finish in 20 ms.
- SDK behaviour is unchanged.

### 2026-10-15, an ignored base branch is reported, #515

- `session new --branch H --base-branch B` checks out an existing local `H` as
  it is, so `B` did not decide the worktree's commit, and nothing said so. When
  `--base-branch` is explicit and the existing branch is at another commit than
  the base (for a remote base, its fetched commit), the result now carries a
  `base_branch_fallback` warning whose `detail` names both commits. The kind is
  the existing one, so released clients still read the session.
- A branch a worktree creates from a base gets no upstream, as before; this is
  now documented, with `git push -u origin <branch>` as the way to set one.

### 2026-10-15, own overlay name selects the local daemon, #540

- `--host <name>` and `<name>/<session-id>` targets resolve this machine's own
  overlay name to the local daemon, like `local`, without dialing the mesh.
  The identity comes from the overlay client's own status (`NetBird` root `fqdn`
  and `netbirdIp`), never from the OS hostname. Exact fqdn, own IP, and short
  name match. A short name shared with a peer is `overlay_host_ambiguous`.
- The CLI resolves the hosts a command selects once, before any host check, so
  confirmations, project path rules, workflow and assistant checks treat the
  own name as local. Those hosts are each target's effective host, a positional
  host (`host inspect`, `host governance inspect`), and `--host` where the
  command reads it; a `--host` the command ignores or an explicit target host
  overrides is never resolved. Literal `local` never runs `netbird`. Each
  distinct selected host other than `local` costs one extra `netbird status`
  read, usually one per command.
- Dynamic completion treats the own name as local too: session targets list
  the local sessions, and workflow lines complete the local store.
- `pohunek host list` and `host discover` show this machine first, routed as
  `local`. The own-identity lookup is bounded by the 5 s connect timeout; past
  it the row is left out and the records print. The `--json` records are
  unchanged.
- The overlay provider contract gains `resolve_selector` and `local_identity`,
  with defaults that keep providers without self-knowledge peer-only.
  `OverlayRegistry::resolve_target` and `local_identities` aggregate them.
  `pohunek_client::names_this_machine` exposes the check. `Client::connect`
  and the TS SDK are unchanged.

### 2026-10-15, service lock terminal handoff before exec, #554

- `pohunek service lock` run in the foreground of a terminal makes its child
  take the terminal for the child's process group before it executes the
  command, as the child of a job-control shell does, and makes the same
  handoff from the lock process after the spawn. A command that reads the
  terminal at once is no longer stopped by `SIGTTIN` before the handoff. The
  lock process took that stop for a job stop, gave the terminal back to its
  own group and stopped itself. A caller without job control, such as a
  script run in a terminal, never continued it, so the lock hung. This was
  the CI timeout of
  `service_lock_terminal::ctrl_z_stops_the_job_and_sigcont_resumes_it_with_the_terminal`.
- When the command cannot be executed, the lock process takes the terminal
  back from the child's group, which has no member left.
- The `test-util` build holds the lock process between the spawn and its own
  handoff while `POHUNEK_TEST_SERVICE_LOCK_HANDOFF_HOLD` names a FIFO.
  Release builds do not contain that hold.

### 2026-10-15, shorter xtask Hermes harness tests, #643

- The controlled fixtures of the xtask Hermes harness tests pause 50 ms
  between typing a prompt and submitting it instead of one second; the fake
  front end needs no settle time. The production `INPUT_SETTLE_WAIT` is
  unchanged.
- The race-window fixtures (a split terminal command line, an unterminated
  panel border, a busy window after Ctrl-C) hold their remaining output for
  0.3 s, fifteen harness polls. The stalled-fixture tests and their
  one-second negative controls keep the 1.5 s stall.
- One healthy refresh carries the sanitized-golden, compatibility-check and
  PTY-descendant-cleanup assertions that three refreshes carried.
- The three TUI diagnosis cases are three tests, so they run in parallel in
  the heavy partition and none is its tail. Under the macOS profile the
  `hermes-pty` test group still runs them one at a time.
- The isolated `rsa-policy` Clippy check is not cached in CI: its target is
  268 MB, and restoring it would add more to the heavy step than the check
  costs in one of its four test threads.

### 2026-10-15, assistant request from stdin, #769

- `pohunek assistant --request-stdin` reads the free-form request from stdin,
  so a caller such as a terminal UI keeps it out of the process argv. The text
  is bounded UTF-8 under the `session new --input-stdin` rules, with
  surrounding whitespace trimmed. It lands in the opening prompt where
  positional words go, so both forms print the same `--json` result. An empty
  or invalid request fails with `cli_usage` before any daemon is started or
  dialed. Request words and the flag are mutually exclusive.
- A remote launch with a stdin request needs `--yes`
  (`confirmation_required`), since stdin cannot also answer the prompt. A
  launch that needs `--yes` (`--json` or a stdin request) is now refused
  before the CLI starts or dials any daemon; `--print-prompt` still needs no
  confirmation.
- `pohunek assistant --request-timeout-ms` and `--meta key=value` work as on
  `session new`: the deadline applies to the creation request only, and the
  metadata is written with the session.
- `docs/public-api.md` documents the creation outcome. The CLI sends one
  `session.new` and never resends it. Only the CLI's own refusals before that
  request prove that nothing was created. `request_timeout`, transport errors
  and typed daemon errors leave the outcome unknown. A caller-chosen `--meta`
  key with `session list --meta-eq` finds the session, or the duplicates of a
  retry, since `session.new` has no idempotency key.

### 2026-10-15, session new waits for the daemon's bounded create, #1014

- Behaviour change in the Rust and TypeScript SDKs: a typed `session.new`
  without an explicit response deadline now waits up to the SDK's creation
  budget, 60 minutes plus 1 second of transport headroom
  (`SESSION_NEW_REQUEST_BUDGET_MS`), on a dedicated connection, instead of the
  5-second default. The budget is a constant derived from the daemon's own
  bounds: worker connection and initialization, up to 10 minutes each, the
  `pre-create` and `post-create` hooks, a host and a repository layer of 5
  minutes each, and, with initial input, the reader grace. Before, only a
  creation with initial input got it. A caller that wants a shorter wait still
  passes one: `--request-timeout-ms`, `with_request_timeout` or
  `requestTimeoutMs`.
- The daemon answers `session.new` only after the project's create hooks, so
  with the 5-second deadline a slow hook turned into `request_timeout` for a
  create the daemon then finished.
- A `session new` that still times out reports the unknown outcome in its
  recovery hint: look the session up by a caller `--meta` key with
  `session list --meta-eq` before retrying, since an empty list does not prove
  that nothing was created and a retry can create a second session.
- The daemon logs `hook started` and `hook finished` at INFO for every hook
  run, with the session id, event, layer, script, timeout, duration and
  outcome.

### 2026-10-15, attach output while the stream's own input is pending, #1021

- An attached terminal froze while its own input waited for the program to
  read it, for example a paste larger than the terminal's input queue into a
  program that does not read. The worker waited for the PTY write inside the
  attach stream's loop and forwarded no output meanwhile. A paste larger than
  the socket buffers made it permanent: the daemon's attach bridge and the
  CLI attach client both waited on their own input write inside the loop that
  forwards output, so each side blocked writing to the other.
- The worker, the daemon bridge and the CLI attach client now keep their
  input write pending across loop iterations and forward output meanwhile.
  Each reads further input only once its pending write completed, which
  keeps the input in order and bounded. An attach write runs in a task of its
  own, so it keeps the PTY execution lock until every byte is accepted even
  if its stream ends first, and a control input waiting behind it keeps its
  stall bound. The CLI sends the detach request for a detach shortcut once
  the input typed before it is written, showing output meanwhile.

### 2026-10-15, preflight profile warnings, #1040

- `pohunekd upgrade-preflight` reports `profile_warnings`: each host profile
  whose detection-manifest override lacks the screen idle rule its `screen_idle`
  base declares. Each profile is judged as the new release loads it, the planned
  pinned text when the release plan rewrites it, else the profile on disk.
- `pohunek service check` and the upgrade print the warnings next to the plan
  and the session verdicts, and `--json` carries the field. A warning never
  refuses the upgrade and needs no `--accept-runtime-loss`.
- The public API document and the update runbook describe the field and the
  code.

### 2026-10-15, hook stderr survives a daemon restart, #1046

- A process a lifecycle hook leaves running with the hook's stderr inherited
  keeps a readable stderr across a daemon restart. Two seconds after the hook
  ended, the daemon hands the stderr pipe to a drain process, `/bin/cat` run by
  absolute path with the pipe as stdin and `/dev/null` as stdout and stderr, in
  its own process group, with a cleared environment and no other inherited
  descriptor. The daemon closes its read end, reaps the drain when the last
  writer is gone, and logs the hand-off at DEBUG with the hook's event and
  script and the drain's PID.
- A restart while the hook runs, or within those two seconds, still gives such
  a process `SIGPIPE` on its next stderr write.
- If the drain cannot start, the daemon keeps discarding the stderr itself and
  logs a WARN naming the hook.

### 2026-10-15, upgrade preflight native reference, #1065

- `pohunekd upgrade-preflight` runs the native reference check resume runs
  before it relaunches a stopped session, and reports a refusal with resume's
  own code and message.
- A stopped session whose conversation is absent from its declared store
  (`agent_native_reference_missing`), or whose stored reference is malformed, is
  `already_not_resumable` when the upgrade keeps the check. A reported reference
  keeps its check only when it is pinned to a package digest, or when a 0.34.0 to
  0.34.2 store (schema 5) is adopted into the official package whose check equals
  the v0.34.2 golden (`V034_GOLDENS`, guarded by a test). Other reported references
  are `would_lose_recovery` with the reason. `service
  upgrade` then does not require `--accept-runtime-loss` for it, and `service
  check` still lists it.
- A relocated store (`agent_native_store_relocated`) or a store-condition
  refusal is `would_lose_recovery` with resume's code: the running release may
  detect those differently, so the preflight cannot prove it refuses too.
- A session the upgrade adopts from the built-in runtime stays
  `would_lose_recovery` when its reference check fails, unless the v0.34 identity
  above proves the check.
- The preflight budget bounds the native reference checks as well as the version
  probes. A stopped session whose reference is not checked before the budget is
  spent is `would_lose_recovery` with `reference_unverified_within_budget`; the
  detail gives the count of such sessions. `service upgrade` and `service check`
  take `--preflight-budget <SECONDS>` (1 to 300, default 20). The CLI's preflight
  deadline grows with it. The flag reaches only this release's upgrade preflight, not
  a rollback's. An installer user runs `<archive dir>/pohunek service check
  --preflight-budget <SECONDS>` and then `<archive dir>/pohunek service upgrade --from
  <archive dir> --preflight-budget <SECONDS>`; the installer script cannot pass it.
- The upgrade preflight judges with the allowlisted variables the service manager
  gives the daemon job (the systemd user manager's environment on Linux, the
  launchd domain environment on macOS), so a store variable such as
  `CLAUDE_CONFIG_DIR` is read as the job reads it. Variable values never appear in
  the report or in errors.
- A job that records its `PATH` no longer fails when its systemd user manager cannot be
  reached: the check runs without the manager's variables and reports a `warning`. A
  manager that answers unusably, or a job that takes its `PATH` from the manager, still
  fails closed.
- A runtime whose native reference is not checked (`check = "none"`) is not held by the
  preflight budget, since it reads nothing.
- Only an absent user bus (no address, or nothing listening at the socket) counts as an
  unreachable service manager. A bus that refuses the client stays an error. A degraded
  judgement's warning reaches every upgrade, rollback and check output, and a refusal that
  the judgement produced carries it too.
- A malformed session bus address is a configuration error and fails closed; only an
  absent socket (not found, or refused) means no service manager is reachable.
- Every refusal of an upgrade, rollback or check gate that a degraded judgement produced
  carries that judgement's warning.
- An upgrade or rollback whose judgement ran without the service manager's variables and
  covered a stopped session is refused with `service_manager_variables_unread` unless
  `--accept-runtime-loss` is given, since that session's reference was checked at the default
  store location. Without stopped sessions no verdict depends on those variables, so the
  gate passes with the warning. `service check` only warns.
  On macOS a missing launchd `gui/<uid>` domain (no GUI session) degrades the same way.

### 2026-10-15, hook admission waits for an unreachable worker, #1088

- A hook report (active-agent report or release, native-id report, hook
  notification) for a session whose launch pin this daemon does not serve is
  admitted by the hook schema its live worker journaled. When the worker could
  not be asked, because its inspect failed or it was reconnecting, the
  admission fell back to the schema the pin resolves to now, which for an
  unserved built-in pin is none, so a transient read failure refused every
  report. The admission now asks the worker again every
  `WORKER_CONNECT_RETRY` until `worker_connect_deadline` and is decided by
  the schema the worker reports once it is back. Only a worker that answered
  without a known schema, or a session with no worker to wait for (lost,
  conflicted or terminal), falls back to the pin.
- A worker that stays unreachable for the whole bound gets the report refused
  the way a schema refusal is: a notification gets `hook_not_admitted`, and
  the other reports are not recorded. Every refusal of a hook report is
  logged at WARN as `session.hook.report_refused` with the session, the
  operation and the denial or the bound.

### 2026-10-15, live transcript tests arm the watcher, #1131

- The live transcript scenarios in `crates/daemon/src/external/live_tests.rs`
  arm the watcher before they change the tree. A probe transcript is
  rewritten until a live event indexes it. A backend that starts delivering
  events only after it reports the root registered (seen as a macOS CI flake
  of `appending_the_identifying_record_indexes_a_transcript`) no longer makes
  a scenario depend on the periodic pass.
- A wait for the first changes after start is bounded by 10 s, a third of
  `RECONCILE_INTERVAL`, so the pass scheduled at start can never satisfy it.
  State waits, and waits for changes made after the scenario caused a pass,
  keep 30 s: such a pass restarts the stream (#1151).
- Product behaviour is unchanged: a change the backend missed is indexed by
  the next pass, at most `RECONCILE_INTERVAL` later. #1151 tracks the
  `FSEvents` stream restart on every pass.

### 2026-10-15, echo held by an attach or resize output pause, #1133

- An attach handshake and a resize suspend the session terminal's output
  (`tcflow` `TCOOFF`) while they take the terminal snapshot boundary. On
  Linux the line discipline keeps the echo of input it accepts during that
  pause in its pending echo buffer, and resuming output does not emit it. The
  echo then waited until the child itself read or wrote the terminal, which a
  child that does not read its input never does. The worker now writes zero
  bytes to the terminal after every resume, including the last-resort resume
  of an abandoned pause, which emits the held echo. The write does not block
  the resuming operation. While a child's write holds the terminal it is
  refused, and since that write processes pending echo only when it began,
  a helper thread repeats the zero-length write once the terminal is free;
  one such thread at most waits per terminal, and further releases requested
  meanwhile make it write again. A
  failure other than a hung-up terminal logs
  `worker.pty.held_echo_release.failed` at WARN.
- This was the cause of the flaky
  `worker_environment::a_stalled_attach_write_keeps_its_stream_while_control_input_fails_with_input_stalled`:
  its observer attached while the stalled attach write was being echoed.

### 2026-10-15, a stop awaits a killed root the kernel holds, #1137

- `pohunek session stop` no longer fails with `timed out waiting for managed
  process exit` when the session's root process, killed after the stop grace,
  is reaped only a little later, as a real agent CLI at a prompt can be on a
  loaded host. After `SIGKILL` the session worker awaits the root's death for
  a post-kill reap deadline of 3 s (the daemon sends it in the worker's stop
  policy) instead of one more 500 ms grace.
- A root still alive after that deadline fails the stop with the root's state,
  for example `in uninterruptible sleep (D)` or a process group it moved to.

### 2026-10-15, adapter report tests await the recorded state, #1145

- The adapter report scenarios in `crates/daemon/src/session/tests.rs` wait
  for the state the adapters reported instead of reading it once when the
  adapter script finishes. An adapter gives up on its answer after its 0.5 s
  socket timeout, so on a loaded host the script finished before the daemon
  recorded the report (a local full-gate flake of
  `the_public_socket_alone_accepts_adapter_reports_of_a_runtime_with_a_hook_schema`).
- That scenario parks every report until the adapters have given up, then
  waits for the record. It makes the loaded-host order deterministic and
  covers the product rule: the daemon still records a report whose client
  stopped waiting. `session.report_agent` is not cancelled on hangup and does
  not depend on the peer process.
- Product behaviour is unchanged.

### 2026-10-15, upgrade updates Hermes with the job's PATH, #1147

- `pohunek service upgrade` updates the plugin of each marked Hermes home with
  the `hermes` executable on the daemon job's `PATH`: the recorded
  `service.toml` search path, or the systemd user manager's or launchd's
  `PATH` for a job without one. It is the same value the upgrade's runtime
  plan runs with, so the `PATH` of the shell that runs the upgrade does not
  decide which Hermes installation is updated.

### 2026-10-15, release layout scan membership on Linux, #1152

- The Linux release-layout scan (`pohunek_processes` in
  `crates/cli/tests/support/release_layout.rs`) decides that a process
  belongs to the scenario's scope by its executable or working directory
  below the scope, or by a value of its environment below it. The
  environment alone missed a child that `spawn` had just returned:
  `/proc/<pid>/environ` reads empty between the point where `execve` closes
  the spawner's close-on-exec pipe and the point where the new image's
  environment is set up. A probe saw it empty in 553 of 3000 spawns. The miss
  showed as a local full-gate flake of
  `guards::a_running_pohunek_process_outside_the_layout_is_refused`.
- That scenario starts its impostor with an empty environment, the state a
  scan can observe during `execve`. It fails deterministically without the
  executable and working-directory membership.
- Product behaviour is unchanged.

### 2026-10-15, scripted retirement waits for the process to end, #1156

- The daemon tests' scripted supervisor ends a retired job's process the way
  a real retirement does: the call returns once the process no longer runs
  (gone, or an unreaped zombie of the test process), not once `kill -KILL`
  has been sent. `a_partial_retirement_keeps_the_remove_intent_for_reconciliation`
  failed about 1 in 10 runs because the restarted registry still saw the
  killed worker running and, correctly, kept the removal for its background
  re-check.
- Product behaviour is unchanged. A removal intent whose worker still runs at
  startup is `conflict` after `reconcile_workers()` and is finished by the
  background re-check once the worker is gone.

### 2026-10-15, daemon tests that failed under host load, #1162

- The Claude native-recovery scenarios (`health_socket` `native_recovery`)
  wait after each hook report until `session.inspect` shows the reported
  conversation as both the active conversation and the recovery target. The
  hook sends the two in separate fire-and-forget requests with a 0.5 s socket
  timeout, so on a loaded host the reference can arrive later, and a fork
  issued in between was refused as `native_identity_unverified` instead of the
  scenario's `native_identity_uncertain`.
- The 24-seed sweep of `native_supersede` runs as four tests of six seeds
  each, with the same seeds. One test took 80-110 s alone and reached the
  180 s slow timeout under load; each batch takes about 20 s and they run in
  parallel.
- The third failure, `conflict_cleanup`'s partial retirement, is #1156.
- Product behaviour is unchanged.

### 2026-10-14, workflow event reactions, #148

- Workflow plugins gain `[reactions.<name>]`: argv arrays that run once per
  matching public event. Events come from the protocol's event table minus the
  attach stream events. Reactions run without the session origin pair.
- One dispatcher unit per namespace runs the hidden `pohunek workflow
  __dispatch`. It reads the daemon's `subscribe` stream, never starts the
  daemon, and keeps one bounded queue per plugin. A full queue drops events and
  writes one `dropped` run record per overflow episode.
- `[reactions]` owner knobs: `queue_depth`, `reconnect_initial_ms`,
  `reconnect_max_ms`.

### 2026-10-14, final runtime package and workflow plugin docs, #151

- The workflow plugins guide and `docs/public-api.md` describe event reactions
  as shipped (the dispatcher unit's hidden `pohunek workflow __dispatch`), and
  the guide lists `status` among the commands whose plugin id completes.
- `docs/architecture.md` names `pohunek workflow enable`, `disable` and
  `uninstall` as the commands that drive the plugin-unit contract.
- README and roadmap: workflow plugins (#148) and the package documentation
  (#151) have landed; the open children of #139 are follow-ups.

### 2026-10-14, workflow status and service uninstall guard, #326

- `pohunek workflow status [<id>] [--json]` reports each recorded job and
  service of the installed plugins: the native state from the service manager
  (`running`, `stopped`, `failed`, or `unknown` with `state_reason`), the last
  run record, consecutive failures and, for a service, the restart backoff
  delay. It is local and read-only. The JSON shape is in `public-api.md`, the
  fields in the workflow plugins guide's Status section.
- `pohunek service uninstall` refuses with `workflow_units_installed` while any
  workflow plugin has a unit record that names units, before the service
  manager is contacted. An unreadable unit record refuses it with
  `workflow_store_failed`. The guard lives in `service::uninstall` and the
  check in `workflow::units::check_no_units`.
- Completion of `pohunek workflow status` offers the installed plugin ids.

- A supervised run refused before its manifest verified records the unit
  name it was started with, so `workflow status` counts the refusal for that
  unit; status reads the run log under the writer's lock read-only and never
  creates the lock file.

### 2026-10-14, Hermes macOS job on nextest, #361

- The `pinned Hermes compatibility (macOS arm64, model-free)` job runs the
  xtask Hermes harness tests and the real-Hermes `hermes_package` scenario
  through nextest under the `macos` profile instead of `cargo test`, keeps one
  JUnit report per run, and uploads them as `hermes-macos-junit` with a flaky
  summary.
- Under the `macos` profile the tests of xtask's `hermes::tests` run one at a
  time (test group `hermes-pty`): its PTY scenarios take turns on an
  in-process mutex that a process per test cannot share. This also applies in
  the `macOS tier packages (arm64)` job.
- The `hermes_macos` paths filter also gates `.config/nextest.toml`.

### 2026-10-14, macOS lane CI jobs, #361

- The `platform contracts (arm64)` job now lints the nine lane packages with one
  clippy run and runs their tests from one nextest archive: `check-macos --scope
  lane` proves the partition, `run-macos` runs it, the lane packages'
  doctests run natively, and the foreign-owner APFS test runs as root from the
  extracted archive. Its thread count, retries and
  launchd test group come from the `macos` profile.
- The new `macOS tier packages (arm64)` job runs `pohunek-client`,
  `pohunek-netbird`, `pohunek-package` and `xtask` on macOS, with
  `check-macos --scope all` over the 13 macOS packages. It runs on push, the
  daily schedule, and `main` or `nightly` dispatches, never on a pull request,
  and it is a required job of the main and nightly release evidence.
- The `macos` paths filter also gates `scripts/test-partitions`,
  `.config/nextest.toml`, `crates/assistant/**` and `crates/package/**`, which
  the lane's build and test binaries depend on.

### 2026-10-14, macOS test lane partitions, #361

- The macOS test selections are a single nextest archive with two runs:
  `scripts/test-partitions run-macos` runs the nine packages the pull-request
  macOS lane tests, and `run-macos-tier` runs `pohunek-client`,
  `pohunek-netbird`, `pohunek-package` and `xtask`, whose macOS-only code the
  main and nightly tiers cover. `check-macos` proves that every macOS test of
  the 13 packages is in exactly one of the lane, the tier, or a named bucket;
  `check-macos --scope lane` checks the same partition over the nine lane
  packages alone, for the PR lane's archive.
- The `macos` nextest profile sets retries for diagnosis; a flaky result still
  fails the run.

### 2026-10-14, Codex folder trust dialog is blocked, #592

- The official Codex package's detection manifest gains `folder_trust_prompt`.
  The `Trust this folder?` dialog that Codex 0.160 shows for a working
  directory with a project-local `.codex/config.toml` now reads as `blocked`
  from the screen at terminal widths of 30 columns and more. The rule needs
  the dialog's `Folder access` heading line, the question and both choices, so
  a reply quoting the dialog is not read as it. Below that width the question
  wraps and the session keeps the byte-activity fallback.
- The rule for the older first-run wording is unchanged, and the package keeps
  every rule of the v0.34.2 built-in manifest.

### 2026-10-14, stderr tail of a failing create hook, #669

- A failing `pre-create` or `post-create` hook's `hook` warning in the
  `session.new` result ends with `; stderr tail:` and the sanitized last bytes
  of the hook's stderr, marked when cut. The stored session, `session.list` and
  the logs keep the warning without the tail; stdout is never kept.
- The hook's stderr is a pipe the daemon drains. A background process the hook
  leaves holding it gets `SIGPIPE` after a daemon restart (macOS, unsupervised),
  so the daemon logs a WARN naming the hook when the pipe is still open two
  seconds after the hook ended; the runbook explains the fix. #1046 tracks a
  detached sink.
- `service.toml` schema 6 adds the required `[hooks] stderr_tail_bytes`
  (`0..=32768`, `0` disables). Fresh installs and upgrades from schemas 2 to 5
  write 4096.
- The RFC security section (B.3.2), `docs/public-api.md`, the setup guide, the
  worktrees concept and the update runbook describe the capture and its
  exposure.

### 2026-10-14, private TMPDIR per session, #845

- Every launched session gets
  `TMPDIR=<cache_dir>/session-tmp/<store key>/<session id>`,
  a private (0700) directory below the daemon's cache directory, on Linux and
  macOS. It replaces a `TMPDIR` forwarded through the environment allowlist, so
  concurrent agents no longer overwrite each other's temporary files; a profile
  `[env] TMPDIR` keeps its own value and gets no private directory.
- A stop keeps the directory and a resume reuses it; a fork gets its own. Every
  removal deletes it with its content, read-only directories included; a
  failed delete is logged with the path and does not fail the removal.
- New runtime-class errors: `session_tmpdir_unsafe` when the directory or the
  `session-tmp` directory is not a private directory of the daemon's user, and
  `session_tmpdir_failed` when it cannot be created or its path is not valid
  UTF-8. Both fail the launch before a worker starts.

### 2026-10-14, notifications list pages a long history, #854

- `pohunek notifications list` without `--limit` lists every matching record
  however long the history is: it requests pages and follows `next_cursor`,
  so it no longer fails with `protocol framing error: control line exceeded
  maximum length`.
- The daemon answers a `notification.list` result that does not fit one
  control line with `notification_list_too_large` and a hint to page with
  `limit` and `next_cursor`, instead of a response the client cannot read. A
  `--limit` too large for one response reports that error.
- A local or remote daemon of an earlier release, which sends such a result
  as one oversized line, is asked again from the first page in smaller pages
  on a new connection, so a listing never combines records from two
  connections.

### 2026-10-14, argument-conflict usage errors end with a newline, #875

- The human usage error of `--host` together with `--all-hosts` ends with a
  newline, so the shell prompt no longer lands on the same line as the
  message. The `session wait` conflict errors share the same rendering. The
  `--json` form is unchanged.

### 2026-10-14, generic SDK calls route waits to dedicated connections, #878

- `pohunek_client::Client::call` and the TypeScript SDK's `Client.call` send
  `session.wait`, and `session.output` with `wait_ms`, on a dedicated
  connection whose deadline follows the requested wait, as the typed session
  wait and output helpers do. A generic wait longer than the 5 s default
  request timeout no longer fails on the client side, and a wait no longer
  blocks the shared connection.

### 2026-10-14, session capability refusal reasons, #912

- `session list` and `session inspect` responses carry an optional
  `capability_refusals` object, `{resume?, fork?}`, whenever the daemon clears
  a frozen `true` capability. Each value is the error code `session.resume`
  or `session.fork` returns when the session's recovery resolution refuses
  it, computed by the same check that clears the flag. State-dependent or
  per-request refusals come only from the operation.
- The reason is an additive field on `SessionInfo`, which is not strict, so
  0.34 clients on protocol 4 ignore it, and the protocol 3 adapter strips it.
  The capability object stays strict and unchanged; moving the reason into
  `SessionCapabilities` is deferred to the next public protocol bump.
- Human `session list` prints `refused <id>: resume=<code> fork=<code>`, and
  `session inspect` has a `capability_refusals` row.

### 2026-10-14, network-isolated archive smoke on macOS, #929

- The `smoke` job in `.github/workflows/release-evidence.yml` becomes a matrix
  with two runners: `ubuntu-latest` (every Linux daemon archive in a network
  namespace via unprivileged user namespaces or `sudo -n unshare`) and `macos-15`
  (the aarch64-apple-darwin daemon archive under `sandbox-exec` after
  `packaging/macos/verify-signed --adhoc`). On macOS, the consumer runs under
  `sandbox-exec` as a throwaway local user created per run and deleted afterwards.
  That requires passwordless `sudo -n`; without it, the smoke fails closed after
  archive and stage validation. Unix sockets are allowed only under the run's own
  roots and the throwaway user's Codex socket directory. The macOS smoke validates
  that the Apple Silicon daemon archive and every official runtime package it
  carries (`claude`, `codex`, `hermes`, `pi`) can be installed offline and serve
  agents on the target platform. Leftover processes are checked with a weaker
  detection on macOS (#1010).
- The macOS scenarios of `scripts/tests/test_smoke_archive` that require the
  isolated run are skipped on machines without passwordless sudo, and the
  macos-15 `smoke` job runs them with `POHUNEK_SMOKE_TESTS_REQUIRE_ISOLATED_RUN=1`.
  The host probe uses a permission-protected unix socket (mode 0755) and a
  world-connectable one (mode 0777) to verify that the sandbox profile itself
  denies unix sockets, independently of file permissions.
- Documented in `guides/hermes-operator`, `runbooks/install-on-macos`.

### 2026-10-14, darwin runtime packages attested, #929

- `compat/matrix.json` declares `aarch64-apple-darwin` rows for `claude`,
  `codex`, `hermes` and `pi`. The Apple Silicon daemon archive carries
  `runtime/packages` and `runtime/attestations` alongside the signed catalog,
  built and tested natively on macOS 15. The claude lock pins the darwin-arm64
  binary by SHA-256 and enables the aarch64-apple-darwin consumer row.
- Documented in `guides/claude-package`, `guides/codex-package`,
  `guides/hermes-package`, `guides/runtime-packages`, `concepts/runtime-catalog`,
  `concepts/release-bundle`, `runbooks/runtime-package-recovery`, and
  `runbooks/update-after-release`.

### 2026-10-14, far-future subagent claims, #940

- A subagent record's journaled sequence, and a remembered orphan stop's, is
  capped at the worker's `CLOCK_MONOTONIC` reading when the claim arrives, so
  one far-future start or stop no longer freezes the record until the runtime
  ends. Claims from the host's monotonic clock keep their sequence, and
  comparisons use the claim's own sequence.
- Identity claims are ordered per reporting process since #879, so a
  far-future identity claim only orders its own process's later claims.

### 2026-10-14, attach menu rename cursor, #949

- The cursor of the attach menu's rename input sits right after the last typed
  character (after `Name: ` when the input is empty). It used to sit one cell
  to the left, on the last character. The overlay cursor column is now counted
  from the first text cell of its line, so a caller no longer has to know the
  box's padding.

### 2026-10-14, worktree owner in project show, #950

- `project.show` adds the optional `owner_session_id` to each worktree that
  pohunek owns: the session the worktree was created for. It stays after that
  session stops, while `session_id` still names only a live session running in
  the worktree. Both are omitted, never `null`, when they do not apply. The
  human `project show` output flags the owner as `owner=<id>`.

### 2026-10-14, client creates daemon directories owner-private, #962

- `pohunek host discover` creates every missing ancestor of its cache
  directory with mode 0700, so `XDG_CACHE_HOME/pohunek` no longer appears as
  0755 and the daemon no longer refuses worker starts with
  `trusted entry has mode 0o755, expected 0o700`.
- The knowledge bundle materializer (`pohunek assistant` locally, and the
  daemon's `assistant.materialize`), the assistant's runtime snapshot
  directory under `XDG_RUNTIME_DIR/pohunek`, and the worker migration manifest
  writer create their missing ancestors the same way.
- A directory an earlier release already created as 0755 is not repaired:
  the daemon still refuses it, naming the path and the expected mode.

### 2026-10-14, launch cwd check, #984

- A session whose working directory no longer exists when the worker launches
  its agent fails with the new public code `session_cwd_missing` (runtime
  class) and starts nothing. The create then runs the existing compensation:
  the worktree binding and the create record are removed once the worker is
  proven ended, so no agent runs in a directory other than its working
  directory (the process would otherwise fall back to `$HOME`).
- The daemon logs worktree binds and removals at INFO, with the removal reason.
  A hook that exits with a failure status, times out, or cannot be spawned or
  waited on is logged at WARN with its event and status; hook output is never
  logged.

### 2026-10-14, unsupported journal warning once per file, #985

- The daemon logs `worker journal schema is unsupported` at warning level
  once per journal file and schema, and every later read of that file at
  debug level. Each journal scan rereads the journals of every stored session,
  so a host with many old schema-1 journals rotated its daemon log every few
  minutes.

### 2026-10-14, a sweep's last pass can confirm the cleanup, #989

- The daemon's lost-runtime sweep decides the outcome after its last allowed
  pass from a survey that sends no signal. A runtime whose processes forked
  during the earlier passes, so that the last pass ended the last of them, is
  `lost` with its cleanup confirmed instead of
  `runtime_lost_cleanup_unconfirmed`, and its removal is no longer refused.
  Anything still attributed to the runtime, or that could belong to it, keeps
  the cleanup unconfirmed.

### 2026-10-14, agent process tree trust boundary, #1007

- The architecture trust boundary, `docs/public-api.md`, the sessions concept,
  the runtime packages guide and the trust model state that the agent's PTY
  subtree runs as the owner and is not an identity boundary: under
  `NamedForProvider` (`identity-subagent-v1`) any process the launch process
  starts can file the launch process's report, and only `FrozenRootEntrypoint`
  (`identity-v1`) binds the root's report to the root's own kernel peer.
- Authenticated agent identity is tracked in #52 and isolated runtime profiles
  in #88; an assigned Claude launch id is #1036 and the Codex evaluation #1037.

### 2026-10-14, one-step upgrade to the official runtime packages, #1011

- `pohunek service upgrade` moves a host on the built-in Claude Code and Codex
  runtimes to their official packages in one step, without
  `--accept-runtime-loss`: it installs the packages from the new release's
  `runtime/`, pins every host profile over them, adopts the live sessions,
  reinstalls the hooks, and leaves stopped sessions resumable, which relaunch
  from the package digest. `--accept-runtime-loss` now covers only a session the
  new daemon cannot adopt.
- The manual `plugin install` and `plugin profile migrate` steps remain the
  fallback for a host that upgrades without the runtime step.
- The upgrade E2E (`crates/cli/tests/release_upgrade.rs`) starts from the
  previous release's built-in profiles with a live and a stopped Claude session,
  runs the one-step upgrade, and asserts the pins, the adoption, the resume onto
  the package digest and new Claude and Codex sessions through the packages. It
  no longer takes the previous release's runtime directory
  (`POHUNEK_PREVIOUS_RUNTIME_DIR` is gone).
- `cargo xtask release test-runtime` writes a test-only runtime layout with a
  discarded key for the upgrade test; it is not a release input.

### 2026-10-14, one host setup command, #1013

- Bare `pohunek setup` is the one host setup. It installs the service from
  this release when none is installed, plans the official runtime packages
  for the runtimes this host has (`pohunekd ensure-official-runtimes --dry-run
  --detect`), previews every effect and gap, asks once, then installs the
  packages, pins the profiles, installs the hooks of the homes that exist and
  writes the missing config files. A second plan and the daemon's health and
  `doctor` form the verdict.
- `--yes` applies unattended. Without a terminal and without `--yes`, setup
  refuses with `setup_consent_required`; `--json` needs `--yes` as well. The
  refusal changes nothing.
- An installed service of another version is a gap: `pohunek service upgrade`
  runs first. Setup never upgrades.
- A Hermes home without the pohunek marker is a gap with its install command;
  setup never picks its access policy. Gaps do not fail the verdict.
- A marked Hermes home whose plugin is already the selected package's,
  unmodified and enabled, is `current` in the preview and is not written,
  unless the run selects another Hermes package. Setup plans and applies the
  Hermes homes with the `hermes` on the daemon job's `PATH`.
- On a fresh host the plan judges the packages with the `PATH` the daemon job
  will get: the search path the install records, or the user service
  manager's own `PATH` when it records none. Setup stops before any change
  when that `PATH` cannot be read.
- A plan that changes after the service install stops with the service
  installed. The stop names the changes the install engine made, and setup
  changes nothing else; a re-run converges. The install engine's own changes
  (installed packages, pinned profiles, a recorded catalog) are accepted only
  as reported by its runtime step.
- A pending interrupted install stops setup before any change.
- On a host without the service, a gap that the install engine's runtime step
  refuses (a used runtime without a package for this platform) stops setup
  before any change instead of rolling the install back.
- On an installed host, setup reads the plan again after the consent; a host
  that changed while the preview was shown stops the run before any change.
- A package the install left disabled or unselected is enabled and selected.
- The Hermes homes applied are the homes previewed; the apply does not
  rediscover them.
- `pohunek setup config` and `pohunek setup completions` are unchanged; an
  existing config file is never overwritten.
- `--json --yes` prints one result document that carries the full preview.
  A failure after the first change lists the changes made in its message.
- `pohunek service install` renders the runtime step it ran (catalog, packages,
  profiles).
- A Hermes home previewed as an update that cannot be updated at apply time
  fails the verdict. A config write that fails part way names the files it
  created. A catalog to be recorded on an installed host with no official
  package to install is a note, not an effect.
- `pohunek setup --dry-run` prints the plan a run would preview and changes
  nothing; with `--json` its `ok` is that preview object. With `--yes` or a
  setup subcommand it is a usage error. The refusal without a terminal names
  `pohunek setup --dry-run --json`.
- Setup run from an extracted release archive verifies it first with the
  archive's own `packaging/verify-archive`, as `packaging/install-daemon.sh`
  does, and stops before any change on a modified member, a removed
  manifest or verifier, a verifier that is a link or writable by another
  account, or a directory above the release that another account can write
  (a root-owned sticky directory such as `/tmp` may be shared).
- The setup plan reads the installation without side effects: no
  transaction lock probe, no change to the service manager, no file
  created. On a fresh host it reads the `PATH` the user service manager
  gives its jobs.
- Setup installs only the hooks the daemon does not report as current, so a
  run on a ready host applies nothing and a modified hook is still repaired.
- The install quick start, the setup guide and the Homebrew path in the macOS
  runbook use `pohunek setup`.
- The CI job `real systemd supervision` also runs the native setup suite,
  including a fresh host set up from a signed release by one run, and the
  setup binary runs that plan a fresh host with the user manager's `PATH`.

### 2026-10-14, persisted active-agent origin, #1034

- A session record now stores how its active agent became known:
  `active_agent_origin`, either `reported` (an agent named itself through a
  hook report) or `observed` (procwatch recognized its process). An identity
  the session's worker journaled is recorded as `reported`, and startup
  reconciliation restores the origin exactly, also after the worker's claim
  expired. An agent reported under the name of a host profile that matches its
  runtime (for example a `claude` profile with its own `manifest`) therefore
  keeps that profile's detection override after a daemon restart and through
  the next package refresh. It no longer falls back to the pinned definition's
  detector until the agent reports again.
- An agent reported only to the daemon socket, the hooks' fallback when the
  worker cannot be reached, is held in memory and reaches the record only with
  the session's next record write for another reason (a native reference
  report, a stop, a worker becoming unavailable or adopted again). Without
  such a write procwatch restores it as observed after a restart (#1101). A
  worker snapshot projected after such a report replaces it with the worker's
  journaled identity, also when the snapshot was read before the report
  arrived; #1101 tracks both.
- The field is not part of `SessionInfo`, so the client protocol is unchanged.
- Store schema 10, a no-op migration from 9. A record written before it has
  no origin and keeps the inference from the agent's name: an agent named like
  its own runtime counts as observed. That restores an observed nested agent
  of another runtime correctly, but a reported agent under a profile named
  like its runtime, nested or not, restores as observed until it reports
  again.

### 2026-10-14, override readiness fallback, #1039

- A host profile over a `screen_idle` base whose detection-manifest override
  has no idle rule on a screen region loads instead of failing with
  `invalid_profile`. Its sessions use `bracketed_paste` readiness, and
  `pohunek doctor` warns with the fix.
- The runtime packages guide, the Claude package guide and the update runbook
  describe the rule.

### 2026-10-14, agent reports follow the current pin, #1041

- An active-agent report resolves its agent against the session's pinned
  definition, or through the runtime registry for an agent of another runtime,
  and commits under the sessions lock. When a package transaction replaced that
  definition in between (an install, an uninstall or a version change), the
  report now resolves again against the current pin and registry instead of
  committing the replaced definition's detector rules and input readiness. A report that keeps meeting replaced
  pins is ignored after three attempts.
- A procwatch scan is discarded, and procwatch scans again, when a package
  change replaced a definition it matched with during the scan: the session's
  own pin, or the runtime registry that other runtimes' agents resolve
  through. When the pin changes, the
  agents procwatch already observed through the replaced definition move to
  the current one. With no current definition they are no longer attributed
  to the session.

### 2026-10-14, Hermes macOS CI on Hermes inputs only, #1050

- On a pull request, the CI job `hermes-compatibility-macos` ("pinned Hermes
  compatibility (macOS arm64, model-free)") runs only when its `hermes_macos`
  input filter matches. A runtime package selection that picks Hermes, which
  every core change does, no longer starts it. Pushes to `main`, the nightly
  schedule and `main`/`nightly` dispatch still run it, and the Linux
  `hermes-package` job keeps following the selection.

### 2026-10-14, knowledge-log fragments, #1052

- A change records its knowledge-log entry as its own file,
  `log.d/<issue>-<slug>.md`, with one `## Unreleased (<date>, <topic>, #<issue>)`
  heading and a body, instead of editing `log.md`, so independent changes never
  conflict on the log. `cargo xtask docs check` refuses an unreleased section
  in `log.md` and a fragment that breaks the rules.
- The embedded bundle and `cargo xtask docs build` show the fragments at the
  top of `log.md` as `## Unreleased` sections, newest date first, then by
  issue. `scripts/release` files them under `## X.Y.Z` with
  `cargo xtask docs log fold` in the release commit and removes them;
  `cargo xtask docs log migrate` converts a branch that still edits `log.md`.
- The unreleased sections that shipped in 0.31.0 to 0.34.0 now sit under those
  release headings; the sections added since 0.34.2 are fragments.

### 2026-10-14, macOS output of a short-lived root, #1054

- On macOS a session whose agent or shell printed its output and exited
  within about a millisecond of its launch could end with that output empty.
  The worker now starts reading the PTY before it closes its own descriptor of
  the terminal, so the whole output is retained. Linux was not affected.

### 2026-10-14, workflow reactions in the consent review, #1061

- The `pohunek workflow install` consent review and `pohunek workflow inspect`
  show every event reaction: its name, its events, its argv with `argv[0]`
  resolved, and its requested timeout (`reactions[]` in `--json`). A plugin
  that declares only reactions now shows the commands it will run before
  `--yes`.
- `pohunek service uninstall`, refused while the reaction dispatcher unit is
  recorded, names the plugins to disable (the enabled plugins with reactions,
  or every installed plugin when none keeps the dispatcher) instead of a
  `pohunek workflow disable the reaction dispatcher` command that does not
  exist.
- Under a service installation of another pohunek version, disabling or
  uninstalling a plugin with reactions succeeds and stops and removes the
  dispatcher unit, as for a plugin's own units. While another enabled plugin
  still reacts, the command warns that no reaction runs until
  `pohunek workflow enable` installs the dispatcher again from this version.

### 2026-10-14, orphaned session TMPDIR cleanup, #1063

- A session's private `TMPDIR` is `<cache_dir>/session-tmp/<store key>/<session
  id>`. The store key is the first 16 hex digits of the SHA-256 of the metadata
  store's canonical path, so daemons with different stores that share a cache
  directory never share or sweep each other's directories.
- Daemon startup deletes every entry of its own root that no startup evidence
  names. Evidence is a session record, a resume binding, a rejected store line
  that names a session, a worker runtime slot or claimed session (also an
  orphaned worker left running), a service-manager job of the session, or a
  worker journal. A crash, a failed delete
  or a store reset leaves such directories behind, and so does the
  `.pohunek-session-tmp-*` quarantine of an interrupted delete. Each deletion
  is logged at INFO.
- Startup deletes nothing when it cannot tell which directories are orphaned,
  and logs why. A WARN is logged when worker discovery, the service manager's
  job listing (strict, so a job whose inspection raced counts as unknown) or
  the journal scan failed. An INFO is logged when a rejected store line names
  no session.
- The deletion is the same descriptor-bound, identity-checked one a session
  removal uses. An entry it cannot delete, such as a symlink or a regular file,
  is never followed; it is logged as a WARN with its path and kept, and startup
  continues.

### 2026-10-14, reconciled sessions start with their active agent's detector, #1080

- A session the daemon adopts at startup now starts its detector with the
  configuration its restored active agent ran before the restart. A reported
  agent gets its profile resolved through the session's pin, including that
  profile's `manifest` override. An observed agent of the session's own
  runtime gets the pinned definition, and an observed agent of another runtime
  gets that runtime's definition. The launch default applies only when there
  is no active agent, when the session's own runtime is not served, or when
  the active agent's profile or runtime no longer resolves; that last case
  keeps the active agent and logs a WARN
  (`session.active_agent.detector_unresolved`).
  Previously the launch default ran until the client reported again or a
  package change refreshed the session.
- An active agent that a managed hook reports through the session's worker now
  switches the running detector by the same rules as soon as the daemon
  projects the worker's journal, and its release returns the session to its
  default configuration. Previously only a report to the daemon socket or a
  procwatch observation switched it.

### 2026-10-14, version probe kills its group before reaping, #1084

- On Linux and macOS, the agent version probe kills the process group it
  started while the exited probe child still holds the group id, and only
  then reaps it. The probe used to reap first and then signal the group, by
  which time the id could name an unrelated process group of the same user.
- When the probe child is no longer waitable, the daemon reports the probe
  as unavailable and sends no signal, because the pid may already belong to
  another process.

### 2026-10-14, screen readiness for Pi and Hermes, #1094

- The Pi and Hermes packages declare `[input] ready = "screen_idle"`. A
  `session.new` with initial input into either runtime (for Pi, through a host
  profile; a bare `pi` launch takes its first prompt as an argument) writes the
  input only once the agent's idle prompt renders (Pi's idle editor frame, Hermes's idle
  prompt), not when it enables bracketed paste and not when the startup grace
  of a silent agent ends.
- Hermes discards terminal input queued during its start, so an initial input
  written before its prompt rendered could be lost while `session.new`
  reported success; Pi enables bracketed paste before it draws any screen.
- When the prompt does not render within the readiness wait,
  `session.new` fails with `session_input_not_ready`, writes nothing and rolls
  the session back, as for Claude, including for clients without
  `extended_input_ready_wait`.
- A runtime descriptor's `[input]` table may declare `startup_grace_ms`, the
  shortest readiness wait the runtime needs to start. For a client that sends
  `extended_input_ready_wait` the wait lasts up to the longer of it and the
  host's `input.initial_startup_grace_ms`; an older client keeps the host's
  grace. The wait still ends as soon as readiness shows. The Hermes package
  declares 30000 ms because its first start after a `uv` install compiles its
  Python modules before the prompt renders, which takes longer than the 5000 ms
  host default. Daemons of release 0.34.x reject a descriptor with the key.
- Codex keeps bracketed-paste readiness: it reports idle only in its terminal
  title, which `screen_idle` does not count (#1114).
- The runtime packages guide lists the readiness of each official runtime and
  the grace that bounds it; the Pi and Hermes package guides, the sessions
  concept and the setup guide describe it.

### 2026-10-14, process environment writes only in a test's own process, #1119

- `pohunek_test_support::process_env::ProcessEnv::set` and `remove` panic
  unless the calling test runs in a process of its own. A process of its own
  is the child that `process_env::in_own_process()` starts with `--exact`, or
  a `cargo nextest` process per test. Under the single-process `cargo test`,
  a test that changes `PATH`, `HOME` or another variable can therefore no
  longer reach the concurrently running tests that read the environment
  without the lock, such as fixtures whose agents inherit it.
- Every test that changed the process environment starts with
  `if !in_own_process() { return; }`. The daemon agent tests' private runner
  delegates to the shared one.

### 2026-10-14, projector notifications after a daemon restart, #1129

- A daemon-projected notification (`agent_blocked`, `error`, `session_finished`
  and the other kinds the projector derives) carries the source id
  `projector:<session>:<kind>:<epoch>`. The store treats a new notification
  whose source id, provider and event match a stored record that is not
  deleted as that record, and creates nothing. The epoch restarted at 1 in
  every daemon process, so after a restart, including every upgrade, a
  session's first transitions of a kind could match records from before the
  restart that were still stored, and did not surface. The epoch of a session
  and kind now continues from the highest one the store holds; the store is
  read once per session and daemon process, off the async runtime.
- A projector epoch at the counter's limit creates nothing and logs a WARN
  instead of wrapping around to a key already in use.
- A daemon restart publishes the current state of every session again. A
  session's `error` and `session_finished` records are keyed by the runtime
  generation that ended (`projector:<session>:<kind>:g<generation>`), so a
  terminal state replayed after a restart keeps its record, a record the owner
  deleted stays deleted while the store still holds its tombstone, and a
  generation that ended while the daemon was down is reported. A terminal
  record from a daemon that keyed it by epoch reports the generation it was
  created in: it covers a replayed terminal state only when it was created
  after that generation started. A generation whose start is unknown is
  reported. A session without runtime information keeps the epoch key.
- A session that is still blocked after a restart raises no new
  `agent_blocked` record while an unread `agent_blocked` or
  `approval_required` record of the session exists; the store is checked when
  the blocked state arrives, and an idle observation before it does not
  change that. Once the owner read or acknowledged the record, the blocked
  state is raised again; inside the attention dedupe window the store merges
  it into a read record, as it merges any repeated report of one attention
  moment.

### 2026-10-13, built-in Hermes removed from the daemon, #147

- The daemon compiles no Hermes in: the official `pohunek.runtime.hermes`
  package, installed from the release catalog, is the only source of
  `hermes`, as the Codex and Claude Code packages are of `codex` and `claude`.
  The shell is the only built-in runtime. On a host without the package, a
  bare `hermes` is `agent_profile_not_found` and an unpinned `base = "hermes"`
  host profile answers `runtime_not_installed`; each recovery hint names
  `pohunek plugin install runtime/packages/hermes.tar.zst --catalog
  runtime/runtime-catalog.json` (the profile hint then names `plugin profile
  migrate <name>`). While the package serves `hermes`, an unpinned profile
  over it needs that migration.
- The release catalog lists the Hermes package for the Linux x86_64 targets
  only, so a macOS daemon has no source of Hermes (#929).
- A session launched from the built-in Hermes of an earlier release (pinned,
  or a v0.33.0 record without a launch binding) answers
  `runtime_builtin_removed` on resume, fork and every interaction (input,
  attach, resize, rename, metadata) while no package serves `hermes`; its hint
  names the install and `pohunek session new --agent hermes`. With the package
  installed the live session can be used again, resume answers
  `runtime_served_by_package` and fork `agent_fork_unsupported` (Hermes
  declares no native fork). `pohunekd upgrade-preflight` reports every live
  built-in Hermes session as `would_lose_recovery`
  (`runtime_source_not_served`) whether or not it could resume; with the
  package installed before the upgrade a non-resumable one is `adoptable`.
  No earlier release archive carries the package to install before the
  upgrade.
- Disabling or uninstalling the Hermes package leaves `hermes` unserved; no
  official alias has a built-in to fall back to.
- The compiled `hermes-v1` version-probe parser is removed. The compiled
  parsers are `semver-v1` and `semver-line-v1`; a descriptor naming any other
  parser id parses, but its runtime reports `supported: false` and a launch is
  refused with `agent_runtime_unsupported`. The probe sandbox sets no Hermes
  home or Python variables and passes the launch `PATH`.
- `daemon.doctor` checks the embedded manifest only for `shell`
  (`agent_manifest_embedded_shell`); `agent_manifest_embedded_hermes` is gone.
- `package.bind_profile` for a `hermes` base no package serves answers
  `package_profile_target_invalid`; `package_profile_base_builtin` is the
  shell's only.
- A `hermes` process outside pohunek or in the foreground of a shell session
  is recognized only through the installed package's detection manifest.
- The `no_capable_agent` hint names the official `codex`, `claude` or
  `hermes` package install from a release archive whose catalog lists the
  platform, or `--agent` with a capable host profile; a `hermes` on the
  daemon's `PATH` alone does not make the assistant capable.
- The v0.34.2 built-in Hermes descriptor and manifest are frozen as goldens in
  `compat/hermes/builtin-v0.34/`; the package tests compare the package with
  them, and a new clean-host scenario covers the refused bare launch, the
  assistant without a capable agent and the launch after the catalog install.
- `pohunek integration ... --agent hermes` stays the CLI-run Hermes operator
  plugin lifecycle; `hermes` stays a reserved runtime id. The hint of a bare
  `integration install` that answers `integration_no_managed_runtime` names
  only the official packages whose hooks the daemon installs (Codex, Claude
  Code), never `hermes`.
- Documented in `guides/hermes-package`, `guides/runtime-packages`,
  `concepts/agent-profiles`, `concepts/sessions`,
  `concepts/runtime-package-archive`, the `runtime-package-recovery` and
  `update-after-release` runbooks, the index, the source map, `README.md`,
  `runtime-packages/README.md`, `compat/hermes/README.md`, `docs/install.md`,
  `docs/architecture.md`, `docs/cli.md`, `docs/public-api.md` and
  `docs/acceptance/README.md`.

### 2026-10-13, Hermes docs match the package-only runtime, #147

- The Hermes operator guide (and the operator skill generated from it) states
  what the official package carries, the launch, resume and detection
  contract in `runtime.toml` and `detect.toml`, instead of a built-in contract
  it preserves.
- `docs/features.md`, `docs/README.md` and `docs/ROADMAP.md` describe Codex,
  Claude Code and Hermes as served only by their official runtime packages:
  only `shell` is built in, the Linux x86_64 daemon archives ship the
  packages, macOS daemons ship none until #929, and without its package a bare
  runtime is `agent_profile_not_found` and a host profile over it
  `runtime_not_installed`.

### 2026-10-13, Hermes operator plugin only from the package, #147

- The CLI installs and updates the Hermes operator plugin only from the
  installed, enabled and selected official `pohunek.runtime.hermes` package. It
  carries no copy of the operator payload; the core hook reporter stays
  compiled in and is installed beside the package payload.
- Without an installed Hermes package, `pohunek integration install|update
  --agent hermes` fails with `hermes_package_not_installed` (class
  `configuration`) and changes nothing; its recovery hint names the official
  package install. `integration status` and `doctor` report the same code in
  `package_source_error`, and doctor fails `asset_source` with that hint.
  `hermes_package_source_missing` is gone: a removed recorded package is now
  `hermes_package_not_installed` with `package_source_missing=true`.
- An install an older CLI rendered from its compiled copy (marker version 1 or
  2, or version 3 with `source=embedded`) is never migrated automatically.
  Status and doctor report `hermes_package_not_installed` while no package is
  installed, `integration uninstall` removes it without a package, and
  `integration update` after the package install rewrites it with
  `source=package`.
- A macOS host has no source for the operator plugin until darwin packages are
  attested (#929).
- The regenerated operator skill changes the package's operator digest, so a
  package-backed install reports `outdated=true` once the new package is
  installed, until `pohunek integration update --agent hermes` rewrites it.
- The `hermes-operator` guide (regenerated Hermes skill, current bare-status
  wording), the `hermes-package` guide, the source map, `docs/install.md`,
  `docs/cli.md`, `docs/runbooks/hermes-operator-plugin.md`,
  `docs/migrations/hermes-operator-plugin.md`, `docs/architecture.md`,
  `runtime-packages/README.md` and `compat/hermes/README.md` describe it.

### 2026-10-13, workflow plugin store and consent install, #148

- New `pohunek workflow install|list|inspect|enable|disable|uninstall`. A
  workflow plugin is a package archive with a strict `workflow.toml` manifest
  (schema 1, `[plugin]` and `[actions.<name>]` with argv arrays), trusted by the
  digest the owner supplies with `--sha256` and installed only with `--yes`
  after a review of every action's argv.
- The CLI keeps workflow plugins in its own registry under
  `<state>/pohunek/workflow-plugins/` with per-plugin state and config
  directories; the daemon never reads them, and no protocol method, event or
  SDK type was added. Every workflow command is local-only.
- New error codes `workflow_local_only`, `workflow_archive_invalid`,
  `workflow_digest_mismatch`, `workflow_descriptor_invalid`,
  `workflow_plugin_unknown`, `workflow_plugin_installed`,
  `workflow_root_invalid`, `workflow_root_intact`, `workflow_store_busy`,
  `workflow_purge_incomplete`, `workflow_dirs_overlap` and
  `workflow_store_failed`; install without consent returns `consent_required`.
- Changing commands hold `store.lock` in the store root. `uninstall --purge`
  also purges the kept directories of an id without a record, and
  `uninstall --remove-modified` removes installed files that fail
  verification. Config directories live under
  `<config>/pohunek/workflow-plugin-config/`.
- A committed uninstall whose files could not all be deleted, an install whose
  durability is unconfirmed, and a stuck leftover of an earlier purge are
  reported as `warnings` on success; they never decide the result. A purge
  leftover of the plugin being purged keeps the purge failing
  (`workflow_purge_incomplete`). An install whose outcome cannot be read back
  fails with `workflow_install_uncertain` and keeps the plugin directories.
- Running actions, reactions, jobs and services are not yet available.
- Documented in the new `workflow-plugins` guide, the `runtime-packages` guide,
  `safety/trust-model`, the index and the source map, and in
  `docs/architecture.md`, `docs/cli.md`, `docs/public-api.md` and `README.md`.

### 2026-10-13, workflow run, owner knobs and run log, #148 #325

- New `pohunek workflow run <plugin> <action> [-- <args>...]` runs one
  declared action of an installed, enabled plugin in text mode, after
  re-verifying its files. It runs without a shell, in the plugin's `files/`
  directory, with stdin from `/dev/null`, in its own process group, and with
  an environment of exactly `POHUNEK_PLUGIN_ID`, `POHUNEK_PLUGIN_VERSION`,
  `POHUNEK_PLUGIN_DIGEST`, `POHUNEK_PLUGIN_CONFIG_DIR`,
  `POHUNEK_PLUGIN_STATE_DIR`, `POHUNEK_PLUGIN_CONTEXT`, `POHUNEK_CLI`, the set
  `HOME`/XDG roots, and the
  `POHUNEK_SESSION_ID`/`POHUNEK_DAEMON_ID` pair when both are set, so
  `plugin_self_target_denied` still guards the hosting session.
- `POHUNEK_PLUGIN_CONTEXT` is a compact schema-1 JSON object with the plugin
  identity, the invocation (`kind`, `name`, ULID `run_id`), the output mode and
  the CLI version and protocol range.
- Deadlines, per-stream output caps and the `SIGTERM`-grace-`SIGKILL` stop come
  from the new owner knobs file `<config>/pohunek/workflow.toml` (`[limits]`,
  `[logs]`; strict, owner-only, defaults when absent). The CLI also stops the
  action's group when it receives `SIGINT`, `SIGTERM` or `SIGHUP`. Exit
  statuses: the action's own, 124 at the deadline, 125 over a cap, 128 plus
  the signal number when interrupted.
- Every run of an installed plugin, refused or not, appends a content-free
  record to `<state>/pohunek/workflow-plugin-state/<id>/logs/runs.jsonl`,
  except runs that fail before the run log is opened (for example invalid
  owner knobs, an unknown plugin, or a store or path failure).
- The action's output is delivered up to its cap whatever the speed of the
  CLI's own stdout and stderr consumer, unless the CLI is signalled, which
  ends the run as `interrupted`. An output pipe that an escaped descendant
  holds open is cut after one second not spent waiting for that consumer,
  whether the descendant writes or not, with a stderr warning.
- New error codes `workflow_config_invalid`, `workflow_plugin_disabled`,
  `workflow_action_unknown`, `workflow_action_args_refused`,
  `workflow_origin_incomplete`, `workflow_timeout_refused`,
  `workflow_context_too_large`, `workflow_run_log_failed`,
  `workflow_spawn_failed`, `workflow_run_failed`, `workflow_action_timeout`,
  `workflow_output_limit` and `workflow_action_interrupted`. No protocol
  method, event or SDK type was added; `--json` for `workflow run` is not yet
  available.
- Documented in the `workflow-plugins` guide, `safety/trust-model`, the source
  map, `docs/cli.md` and `docs/public-api.md`.

### 2026-10-13, managed hook wording, #151

- The debug-daemon runbook and the public API call the scripts that
  `integration install` writes "the managed hooks" and name the compiled Codex
  and Claude integration handlers as what registers them; neither is called an
  adapter.

### 2026-10-13, shell completion of workflow plugins, #325

- Dynamic shell completion completes installed workflow plugin ids for
  `workflow inspect`, `enable`, `disable`, `uninstall` and `run` (`run` offers
  only enabled plugins), and the actions of the typed plugin for
  `workflow run <id>`, read from its installed `workflow.toml` after its files
  verify. The lookups read the local store only, without the daemon, within the
  completion deadline; they create, lock and write nothing, and a missing,
  corrupt or unsafe store completes nothing silently. A line with a remote
  `--host` completes no workflow plugin, matching `workflow_local_only`. Static
  scripts never read the store.
- `docs/cli.md`, `docs/features.md` and the `setup` and `workflow-plugins`
  guides describe it.

### 2026-10-13, workflow run --json and verbatim arguments, #325

- `pohunek workflow run [--json] <plugin> <action> [<args>...]`: every
  argument after `<action>` reaches the action verbatim, including `--json`,
  `--`, `-h`, `--host` and empty strings, so pohunek's own options go before
  `<plugin>`. A `--` after the action is an argument for the action.
- `--json` captures the action's stdout up to its cap and prints one CLI
  envelope whose `ok` is the action's single JSON document byte for byte;
  stderr still streams. The context's `output` is `json`.
- New error codes, each an envelope without output bytes:
  `workflow_action_failed` (non-zero exit or a signal the CLI did not send;
  carries `exit_code` and `signal?`, and the CLI exits with the action's
  status), `workflow_output_invalid` (exit 0 but stdout is not exactly one
  JSON document, including arrays and objects nested deeper than 126 levels;
  exit 1) and `workflow_output_incomplete` (exit 0 but an
  escaped process held stdout open; exit 1). The run log gains the outcomes
  `output_invalid` and `output_incomplete`.
- The workflow plugins guide, `docs/public-api.md` and `docs/cli.md` describe
  it.

### 2026-10-13, workflow plugin jobs and services, #326

- `workflow.toml` accepts `[jobs.<name>]` (`argv`, `every_s`, optional
  `timeout_ms`) and `[services.<name>]` (`argv`); the consent review and
  `inspect` show them.
- `pohunek workflow enable` installs and starts one user service manager unit
  per job and service (systemd user units, launchd agents) of the
  `pohunek service` installation; `disable` and `uninstall` stop and remove
  them. New error codes: `workflow_service_not_installed`,
  `workflow_unit_refused`, `workflow_units_failed`, `workflow_unit_stale`,
  `workflow_unit_unknown`.
- Every unit runs the hidden `pohunek workflow __exec`, which refuses residue
  units, records `overlap_skipped` while a run is active, runs a job once its
  wall-clock `every_s` has passed, applies a service's restart backoff, never
  forwards the origin pair or the service manager's environment, and stops
  the run's process group on `SIGTERM`, then exits 0 so a unit its service
  manager stopped reads as stopped (the run is recorded `interrupted`).
  systemd plugin units use `KillMode=mixed`, so a stop signals only the
  trampoline; the final `SIGKILL` still reaches every remaining process.
  Uninstall applies its own refusals first, then disables the plugin and
  removes its units; it fails while a unit cannot be removed. Installing a
  recorded archive again never touches its units. An enabling install
  checks everything and takes `units.lock` before it records the plugin; a
  failure after that leaves it installed disabled
  (`workflow_installed_disabled`, with the failure's code as `cause`, and
  a message that says whether undoing the failed enable succeeded). A job outside the `[jobs]` bounds is
  refused at run time too.
- Each plugin with units keeps the names of its units in a unit record under
  the store root. `disable` and `uninstall` stop and remove the recorded units
  as well as the manifest's, so a plugin whose installed files fail
  verification no longer leaves running units behind; `uninstall` refuses to
  remove the plugin record while units are still recorded.
- `enable` refuses an installation recorded for another pohunek version
  (`workflow_service_not_installed`, naming both versions), since its units
  would run that version's trampoline.
- A stop request that arrives while the trampoline verifies a unit ends the run
  before its child starts. A stop request during a refused service's restart
  pacing ends the trampoline as stopped (exit 0), not as a refusal. Run records gain the kinds `job` and
  `service` and the field `backoff_ms`.
- New owner knobs `[jobs]` (`timeout_default_ms`, `timeout_max_ms`,
  `min_interval_s`, `check_period_s`) and `[services]`
  (`restart_initial_ms`, `restart_max_ms`, `stable_after_ms`).
- The architecture concept says which login units exist besides the daemon.
- `uninstall` of an id with no record and no store root is refused as
  `workflow_plugin_unknown` before any lock is taken, the same answer as an
  existing store without the record. With `--purge` the unit lock is taken in
  creating mode, so an enabling install cannot record the plugin between the
  check and the removal; a recorded plugin is still disabled and its units
  removed first.
- A failure while marking a fresh plugin enabled that may have written the flag
  disables it again before `workflow_installed_disabled` is reported.
- `enable` records a plugin's unit names before it marks the plugin enabled.
  When the unit record cannot be written, the command fails with
  `workflow_store_failed` and changes nothing: an enabled plugin whose units
  keep running stays enabled instead of being disabled.
- The unit record is bounded at three times the manifest limit (192 KiB), so the
  record of a manifest at its size limit with short service names is read.
- On macOS, `__exec` ignores an inherited `XDG_RUNTIME_DIR` when the
  installation records the default runtime root
  (`/private/tmp/pohunek-<uid>`), and leaves it out of the run's environment;
  before, every run of such an installation was refused after
  `launchctl setenv XDG_RUNTIME_DIR`.
- A service's restart backoff counts the child's own lifetime, from its spawn
  until it ends, so verification and output draining never make an
  immediately failing service look stable; a run that started no child is
  short.
- Without a recorded `pohunek service` installation, `disable` and `uninstall`
  fail with `workflow_units_failed` and keep the unit record when the service
  manager cannot be reached while units are recorded.
- Known limit on macOS: when `__exec` itself is killed with `SIGKILL`, the
  run's process group can survive the unit; a later start can run alongside it
  and `disable` and `uninstall` do not find it. The guide documents it; a
  reconcile mechanism is a follow-up.

### 2026-10-13, declared input readiness for Claude, #844

- A runtime descriptor's `[input]` table may declare `ready`:
  `bracketed_paste` (the default when absent) or `screen_idle`. `screen_idle`
  counts only the screen-sourced idle evidence of the detection manifest, not
  the bracketed-paste enable signal.
- The Claude package declares `ready = "screen_idle"` because Claude drops
  pastes sent between its bracketed-paste enable and the first rendered prompt
  box. Its version stays `1.0.1`; the version bump gate passes against the
  released `1.0.0`.
- A `session.new` with initial input into a `screen_idle` runtime that shows no
  readiness by the end of `input.initial_startup_grace_ms` fails with
  `session_input_not_ready` (runtime class), writes nothing, and rolls the
  session back. It uses the full grace even from clients without
  `extended_input_ready_wait`.
- A descriptor carrying `ready` is rejected by daemons of release 0.34.x, which
  parse `[input]` strictly.
- The runtime packages guide, the Claude package guide, `docs/public-api.md`,
  `docs/architecture.md`, the sessions concept and the setup guide describe it.

### 2026-10-13, bounded PTY input stall, #867

- `service.toml` schema 5 adds the required key `[input]`
  `write_stall_timeout_ms` (3000 on fresh installs). It is a whole millisecond
  count in `1..=600000`. A missing key makes the daemon refuse startup. Schema
  4 files, the layout `main` released before this change, are now an upgrade
  source like schemas 2 and 3: `pohunek service upgrade` migrates them to
  schema 5 keeping every recorded value and their `[waits]` table, and assigns
  the 3000 ms stall bound. Schemas 2 and 3 migrate straight to schema 5 with
  the same default. The value is the longest time one `session.input` or initial input
  write may wait without the agent reading a byte; it restarts after every
  accepted byte. Attached terminal input is not bounded.
- The daemon sends the bound to each worker in `Initialize`. A write that
  exceeds it fails with the new public code `session_input_stalled` (runtime
  class). Part of the plan may already have been written. `session.new` with
  initial input then removes the session it was creating; `session.input`
  leaves the session running.
- A worker that negotiated a protocol below 7 receives no bound and keeps
  unbounded writes.
- The 3000 ms default is below the client's 5 s request deadline, so a
  fire-and-forget caller receives the typed error instead of `request_timeout`.

### 2026-10-13, native-id report attestation, #868

- `session.report_native_id` on the local control socket is attested with the
  caller's kernel peer credentials before any session state is read. Only the
  session's PTY root itself is admitted; a descendant, another user, a remote
  TCP caller, or any report for an `identity-v1` (Hermes) session is refused
  with the error `native_report_unattested` (class `runtime`) and logged once as
  `session.native_report.unattested` with a `reason`.
- Shipped Claude and Codex hook fallbacks to this method are refused, because
  their hooks are children of the agent; the native reference is recorded
  through the worker hook socket. The hook scripts are unchanged.
- The rule is in the public API ("Native-id report attestation"), the sessions
  concept and the trust model.

### 2026-10-13, identity sequences ordered per process, #879

- The worker orders identity claims and releases per reported process (pid and
  kernel start identity): a report must be newer than every claim and release
  accepted for that same process, and reports of different processes are never
  compared. A descendant's report of its own process, even at
  `sequence = u64::MAX`, no longer makes the launch root's later reports stale,
  so the root still establishes the launch identity and updates its native
  reference. Across processes the latest accepted claim is the active identity.
- The worker keeps the sequence of every live reporting process and never
  evicts it, so a released process cannot become active again through its own
  older claim delivered late. Marks of exited processes are pruned when room is
  needed; the PTY root and the launch process are always kept. When live
  reporters besides those two fill the bound (16), a further process's report
  is refused with the new reason `identity_reporters_exhausted` until one
  exits; an exited process its parent has not reaped counts as exited. Before
  the launch identity exists, a launch agent's report with a native reference
  is verified as a launch claim even at the bound and kept only when it
  establishes the launch identity, so a full table cannot keep a descendant
  launch process from being pinned; a deferred verification keeps only its
  pending launch claim for the retry. A release answered `ok: false` still
  orders its process's later claims.
- A report of another process accepted as the active identity clears the
  background owner's snapshot (`agent_changed`) whatever its sequence, since
  sequences of different processes are never compared; the clear raises the
  background mark to the worker's own clock, so an owner snapshot taken before
  the report and delivered after it stays stale.
- `docs/public-api.md`, the runtime packages guide and the sessions concept
  state the per-process order; the sequence contract is a value strictly
  increasing across every hook event that reports the same process.

### 2026-10-13, package version bump gate, #894

- `cargo xtask package bump` fails a runtime package whose archive differs
  from every released archive of its runtime while its `runtime.toml` version
  is not strictly above the released versions (semver). It reuses the verdicts
  of `package baseline` and adds the decisions `pass`, `undecided` (catalog
  absent) and `fail` (`version_not_bumped`, or a catalog that does not verify).
- The `package version bump` pull request job in `ci.yml` compares with the
  last published release at or below the workspace version.
- The Codex package is bumped to `1.1.0` for its transcript tree declaration.
- The rule is in the runtime packages guide; the archive concept describes
  `version_not_bumped` and the bump decisions.

### 2026-10-13, profile list reports unserved bases, #958

- `pohunek plugin profile list` reports `builtin` only for a profile over
  `shell`, the one runtime built into the daemon. An unpinned profile whose
  base no installed package serves, such as `codex`, `claude`, `hermes` or a
  third-party runtime, is `package_not_installed`: its launches fail with
  `runtime_not_installed`. The detail names the official package install for
  an official alias, a package that serves the base otherwise, then
  `pohunek plugin profile migrate <name>`.
- When an installed package that may serve such a base cannot be loaded (its
  root or descriptor fails verification; for an official alias only that
  alias's official package, `pohunek.runtime.<alias>`, counts), the profile is `package_unavailable` and the
  detail names `pohunek plugin doctor`, which reports the fault and its
  repair.
- The `runtime-packages` guide, the `runtime-package-recovery` runbook and
  `docs/cli.md` list the state.

### 2026-10-13, package archives decoded only after digest authorization, #967

- `plugin install --catalog` looks the raw archive digest up in the verified
  catalog before it decompresses anything. Bytes the catalog does not list,
  such as a corrupted copy of a listed archive or a file that is not an
  archive, are refused with `package_untrusted` instead of
  `package_archive_invalid`; nothing reaches the zstd decoder. A listed archive
  that does not decode is still `package_archive_invalid`, and the identity,
  version, core and platform checks after decoding are unchanged.
- `package::archive_digest` computes the package digest of raw bytes without
  decoding them.
- `.cargo/audit.toml` ignores RUSTSEC-2026-0344 (ruzstd 0.9.0 decoder memory
  blow-up) until the package format migration to ruzstd >= 0.9.1 (#969).
- The `runtime-package-archive` and `runtime-catalog` concepts, the
  `runtime-packages` and `hermes-package` guides and the
  `runtime-package-recovery` runbook describe it.

### 2026-10-13, package archive format on ruzstd 0.9.1, #969

- Package archives are encoded and read with `ruzstd` 0.9.1, which fixes
  RUSTSEC-2026-0344; the `.cargo/audit.toml` ignore is gone. The encoder writes
  other bytes than 0.9.0 for package-sized inputs, so this is a format
  cut-over: 0.35 refuses archives built before it (including the 0.34.x
  release archives) and 0.34.x refuses archives built by 0.35. Installed
  packages keep working across the upgrade, because the registry never
  re-reads archive bytes. `NonCanonicalCompression` names the rebuild.
- An encoder-drift guard pins the digests of a large generated sample and of
  the released v0.34.2 Claude package's content under the current encoder, and
  a test pins that the v0.34.2 archive is refused.
- The release upgrade test takes the previous release's archive digest from
  its raw bytes and its descriptor from a plain decode, and after the upgrade
  the head links a probe package directory, so its daemon installs an archive
  of the head's format next to the pinned package.
- `concepts/runtime-package-archive.md` describes the format generations.

### 2026-10-13, verify-inventory checks package digests before decoding, #972

- `cargo xtask release verify-inventory` takes a required `--anchor <file>`
  and refuses with a trust-anchor mismatch when any daemon archive ships
  another anchor, before it verifies the catalog or reads a package. Each
  top-level package archive is then compared with the digest of the catalog
  verified under that anchor before it is decompressed, so a tampered package
  is refused as a package-archive mismatch without reaching the decoder
  (`concepts/release-bundle.md`).

### 2026-10-13, doctor hints for unloadable packages, #991

- `pohunek plugin profile list` reports a pinned profile whose pinned digest is
  installed but cannot be loaded as `package_unavailable`, when the pin names
  that digest's package. The detail names `pohunek plugin doctor` only; the
  migration is not offered because the pin already names the digest. A
  loadable package that serves another runtime, or a pin naming another
  package than the digest's, is still `needs_migration`.
- An unpinned profile over an official alias (`codex`, `claude`, `hermes`)
  whose official package (`pohunek.runtime.<alias>`, matched by package id) is
  installed but was left out of the runtime registry after a rebuild is refused with `runtime_incompatible` (it was
  `runtime_not_installed`). Its recovery names `pohunek plugin doctor` and the
  migration after the repair, not the install, which the package's root or
  identity check refuses. The refusal is decided from the package report the
  registry rebuild records; no protocol code was added.
- The runtime packages guide, the runtime package recovery runbook and
  `docs/public-api.md` list the new states and refusal.

### 2026-10-12, built-in Codex removed, #145

- The daemon compiles no Codex in: the official `pohunek.runtime.codex`
  package, installed from the release catalog, is the only source of `codex`.
  On a host without it, a bare `codex` stays `agent_profile_not_found`, an
  unpinned `base = "codex"` host profile and `integration.install` for
  `codex` answer `runtime_not_installed`, and each recovery hint names
  `pohunek plugin install runtime/packages/codex.tar.zst --catalog
  runtime/runtime-catalog.json` (the profile hint then names `plugin profile
  migrate <name>`). The `agent_profile_not_found` hint of any other name lists
  `shell` or an installed runtime (`pohunek plugin list`) instead of fixed
  runtime ids.
- New `runtime/runtime_builtin_removed`: resume, fork and every mutation
  (input, attach, resize, rename, metadata) of a session launched from the
  built-in runtime of an official alias this daemon does not compile in
  (pinned, or a record without a launch binding) while no package serves the
  alias. Its hint names the package install and a new session. With the
  package installed, the live session can be used again and resume and fork
  answer `runtime_served_by_package`.
- `pohunekd upgrade-preflight` judges every live session's runtime source with
  the mutation guard, whatever its resume and fork capabilities: a session
  whose runtime resolves nowhere is `would_lose_recovery`
  (`runtime_source_not_served`), and its detail says every interaction is
  refused until the runtime (for a removed built-in, the official package) is
  installed.
- The release catalog lists the Codex package for the Linux x86_64 targets
  only, so a macOS daemon has no source of Codex (#929).
- `integration.status`, `integration.doctor` and `integration.uninstall` that
  name `codex` work without the package: the core-owned Codex handler keeps
  the config home of the earlier built-in, so leftover pohunek hooks are
  reported and removed, in the runtime's home and, with `profile` or
  `all_profiles`, in the homes of host profiles over `codex` (found from each
  profile's `base` and `[env]`, since such a profile does not resolve). Bare
  status and doctor cover only served runtimes.
- A foreground `codex` in a shell session and an unmanaged `codex` are
  recognized only through the installed package's detection manifest and
  `[transcripts]` table; without the package they are not observed as Codex.
- `daemon.doctor` checks embedded manifests only for `shell`, `claude` and
  `hermes`. `pohunek integration --agent` offers `claude` and `hermes` as
  built-in names; `codex` is accepted as a runtime id.
- Documented in `guides/codex-package`, `guides/runtime-packages`,
  `concepts/agent-profiles`, `concepts/sessions`, the
  `runtime-package-recovery` and `update-after-release` runbooks, the source
  map, `docs/install.md`, `docs/architecture.md`, `docs/cli.md` and
  `docs/public-api.md`.

### 2026-10-12, built-in parity goldens, #145

- `compat/codex/builtin-v0.34/` and `compat/claude/builtin-v0.34/` hold golden
  copies of the built-in Codex and Claude descriptors and detection manifests
  of release 0.34.2. The package tests compare the official packages with these
  goldens; the 0.34.2 descriptors declare no transcript tree. While the daemon
  embeds the built-ins, the tests also require the built-in manifests to equal
  the goldens byte for byte and the built-in descriptors to be the goldens
  followed by the package's transcript tree. Documented in the `codex-package`
  and `claude-package` guides and the source map.

### 2026-10-12, missing program before the version probe, #145

- `session.new`, `session.resume` and `session.fork` resolve the runtime's
  program before any version probe, for every runtime but a bare host shell
  (whose configured shell command its launch resolves), and answer
  `runtime/agent_binary_missing` when it does not resolve to an executable,
  before any session, worker, worktree or recovery write. That refusal's
  message and recover hint name only the program's file name. Only an
  executable that exists but that the probe rejects (another release,
  unparseable output, a failing or timed-out probe, an unknown parser) answers
  `agent_runtime_unsupported`. A missing Hermes executable, and a missing
  executable of a package runtime that declares a probe (the official Codex
  and Claude packages), therefore answer `agent_binary_missing`. Documented in
  `concepts/agent-profiles`, `concepts/sessions`, the
  `debug-session-runtime` runbook and `docs/public-api.md`.

### 2026-10-12, notification providers by runtime id, #145/#146

- Notification source ranking follows the hook schemas: a provider ranks as a
  provider hook exactly when a registered hook schema lists it as an identity
  provider (today `shell`, `codex`, `claude`, and `hermes`, matched
  case-sensitively). Hook admission on `notification.create` checks those
  providers against the session's hook schema and additionally admits a
  session's own runtime, which does not change its rank. Hermes and shell hook
  reports therefore rank above user/external reports: inside the debounce
  window a pending Hermes attention report is kept over a later user report
  for the same session, and inside the dedupe window it upgrades a projector
  record and dedupes against an earlier provider-backed record.
- The default notification policy has no per-provider entries; every provider
  uses the base `enabled` flags until an override is set. A policy file the
  daemon already persisted keeps the entries it holds, so on such a host a
  `--provider default` toggle does not reach `codex`, `claude`, or `hermes`
  until their own entries are changed too (`policy set --provider <runtime-id>`).
  The SDK testkit fixture daemon serves the same empty default.
- `notification.create` judges a session hook claim by the provider name the
  record is stored and ranked under, and refuses with `hook_not_admitted` a
  raw provider that only normalizes to a hook provider (for example
  `"hermes\u0007"` or `" codex"`), so such a spelling cannot bypass the
  session's hook schema.
- `pohunek notifications policy set --provider` accepts `default` or any valid
  runtime id, installed or not (for example `pi`); a new override starts from
  the base flags. Documented in `concepts/sessions`, the `debug-daemon`
  runbook, `docs/cli.md`, and
  `docs/public-api.md`.

### 2026-10-12, sessions pinned to an unserved runtime source, #145/#146

- A session is observed, resumed and forked only through the runtime source its
  launch pin names. A built-in pin is served only by a built-in of the same
  package identity the registry serves now; an official package that serves the
  alias, a built-in the daemon does not ship, or a package digest that is not
  installed never stands in. Resume and fork of a built-in session of an alias
  an official package serves fail with `runtime_served_by_package`, whose hint
  names `pohunek session new --agent <runtime>` and states that no session is
  rebound. A live session adopted at startup with such a pin is observed with
  the generic shell manifest (or its host profile's manifest override), its
  persisted input framing and its journaled hook schema; hook reports and
  processes naming its runtime are not attributed through the package, and
  stay unattributed until the next restart even when the pinned source is
  served again. A worker of a release before 0.34.0 journals no hook schema, so
  such a session with a journaled active identity is not adopted
  (`worker_identity_unverified` in the preflight, a `conflict` with
  `active_identity_provider_invalid` at startup). A recovery binding whose
  launch spec the migration left unresolved is completed only from the
  definition its pin names and otherwise stays unrepaired with a
  `native_recovery` warning. The upgrade preflight reports such a session
  `would_lose_recovery` with `runtime_source_not_served`. Documented in
  `guides/runtime-packages`, `concepts/runtime-package-archive` and
  `update-after-release`.

### 2026-10-12, built-in Claude removed, #146

- The daemon compiles no Claude in: the official `pohunek.runtime.claude`
  package, installed from the release catalog, is the only source of
  `claude`, as the Codex package is of `codex`. On a host without it, a bare
  `claude` is `agent_profile_not_found`, an unpinned `base = "claude"` host
  profile and `integration.install` for `claude` answer
  `runtime_not_installed`, and each recovery hint names `pohunek plugin
  install runtime/packages/claude.tar.zst --catalog
  runtime/runtime-catalog.json` (the profile hint then names `plugin profile
  migrate <name>`). The built-in runtimes are `shell` and `hermes`.
- A session launched from the built-in Claude of an earlier release (pinned,
  or a v0.33.0 record without a launch binding) answers
  `runtime_builtin_removed` on resume, fork and every interaction (input,
  attach, resize, rename, metadata) while no package serves `claude`; with the
  package installed the live session can be used again and resume and fork
  answer `runtime_served_by_package`. `pohunekd upgrade-preflight` reports
  such a session as `would_lose_recovery` (`runtime_source_not_served`)
  whether or not it could resume.
- The release catalog lists the Claude Code package for the Linux x86_64
  targets only, so a macOS daemon has no source of Claude Code (#929).
- `integration.status`, `integration.doctor` and `integration.uninstall` that
  name `claude` work without the package: the core-owned Claude handler keeps
  the config home of the 0.34 built-in (`CLAUDE_CONFIG_DIR`, else
  `~/.claude`), in the runtime's home and, with `profile` or `all_profiles`,
  in the homes of host profiles over `claude`. Install never resolves through
  it.
- New `configuration/integration_home_ambiguous_owner`: an integration request
  without `agent` that reaches one config home through one handler for
  several runtimes (a package declaring `claude-hook-v1` and Claude's
  `[config_home]` under another runtime id) attributes the home to none of
  them. Each runtime reports the error for it: a bare install fails,
  `all_profiles` lists it in `failed`, and status and doctor report the home
  unresolved. A request naming `agent` addresses only that runtime.
- A foreground `claude` in a shell session and an unmanaged `claude` are
  recognized only through the installed package's detection manifest and
  `[transcripts]` table; without the package they are not observed as Claude
  Code.
- `daemon.doctor` checks embedded manifests only for `shell` and `hermes`.
  `pohunek integration --agent` offers only `hermes` as a built-in name;
  `claude` and `codex` are accepted as runtime ids.
- New `runtime/integration_no_managed_runtime`: a bare `integration install`
  on a host where no served runtime has daemon-managed hooks; its hint names
  the official package install and then `integration install --agent
  <runtime>`. A bare install resolves every selected home before it writes,
  so an ambiguous home refuses the request with no home written. The status
  warning of an unresolved home carries the error's hint; the integration
  doctor reports an ambiguous home as `config_root_invalid` with a remediation
  naming `--agent`, and `daemon.doctor` as a warning.
- `--all-profiles` covers host profiles over a served runtime that do not
  resolve (unpinned over a package-served alias, or pinned to a version that
  is not installed): status, doctor and uninstall reach their own `[env]`
  home, and install lists them in `failed`.
- A shell session hosting an agent whose runtime is not served (a Claude
  identity reported by leftover hooks without the package) stays mutable:
  only the session's own runtime must resolve; a nested agent is checked
  against the runtime-id grammar.
- The `no_capable_agent` hint names the official `codex` or `claude` package
  install from a release archive, a supported `hermes` on the daemon's PATH, or
  `--agent` with a capable host profile.
- Documented in `guides/claude-package`, `guides/runtime-packages`,
  `concepts/agent-profiles`, `concepts/sessions`, the
  `runtime-package-recovery` and `update-after-release` runbooks, the source
  map, `README.md`, `docs/install.md`, `docs/architecture.md`, `docs/cli.md`
  and `docs/public-api.md`.

### 2026-10-12, Claude reporter templates, #146

- The Claude reporter scripts are core-owned templates with the
  `@POHUNEK_AGENT_ID@` and `@POHUNEK_AGENT_NAME@` placeholders, rendered by
  the Claude handler from the runtime id `claude` and the name `Claude` like
  the Codex scripts. The installed bytes are unchanged (recorded in
  `crates/daemon/src/integration/reporter_golden/claude-*.sh`), so installed
  integrations stay current. Documented in the runtime packages guide.

### 2026-10-12, upgrade test through the Claude package, #146

- The release upgrade test installs the official Claude package from the
  previous release's archive under its signed catalog, pins the fake-agent
  profiles with `plugin profile migrate`, and starts its session from the
  package; it asserts the preflight judges that session `adoptable` and that
  the store keeps the same package pin across adoption and `resume`.
  `scripts/upgrade-test` refuses a previous release without
  `runtime/packages/claude.tar.zst` (older than v0.34.2) or with an expired
  catalog. The `update-after-release` runbook describes moving Claude and Codex
  work onto the official package before upgrading to a release that does not
  serve the built-in runtime.

### 2026-10-12, Hermes gate and smoke from the package, #147

- Every `pohunek integration` result for Hermes reports `provenance`: the
  source the installed plugin's ownership marker records, as
  `{"source":"package","package_id":"pohunek.runtime.hermes","package_digest":"sha256:<hex>"}`
  or `{"source":"embedded"}`, and `null` when no managed plugin is installed
  (also after `uninstall`). The human form prints a `source:` line. Documented
  in the `hermes-operator` guide (regenerated Hermes skill).
- `cargo xtask hermes compatibility` takes `--bin-dir` (the directory of
  `pohunek`, `pohunekd`, and `pohunek-sessiond`) and `--package` (the Hermes
  runtime package archive, built by `scripts/build-hermes-package`) in place of
  `--pohunek-bin`. It copies the binaries into a private prefix with a
  throwaway trust anchor, signs a catalog for exactly that archive, starts the
  copied daemon, installs the package with `pohunek plugin install --catalog`,
  and fails unless install, status, and doctor report the archive digest as
  the provenance (`source=embedded` fails). `cargo xtask hermes
  throwaway-catalog` writes such a catalog and its anchor for the release
  smoke. `packaging/smoke-hermes-plugin-release` ships in the daemon archive
  and takes the extracted daemon archive, the package archive, the catalog, the
  anchor, and the Hermes executable. Documented in the source map.

### 2026-10-12, Hermes lock shape, #147

- `compat/hermes/compatibility-lock.json` has the official-package lock shape:
  `schema`, `package` (`pohunek.runtime.hermes`), `runtime` (`hermes`), an
  `upstream` object of kind `provisioner` that holds the release and its source
  provenance, `supported` (`[0.20.0, 0.20.1)`, equal to the package descriptor's
  version probe range) and `verified_with`; the launch contract is `launch`.
  `cargo xtask compat attest` and `compat verify` read it like the npm locks
  (`compat matrix-check` once `runtime-packages/hermes/` exists), and
  `compat stage-upstream` refuses it for having no `upstream.npm`.
  Documented in `concepts/upstream-staging` and `compat/hermes/README.md`.

### 2026-10-12, Hermes package in runtime-packages, #147

- `runtime-packages/hermes/` is the official Hermes runtime package:
  `runtime.toml`, `detect.toml`, the closed operator sidecar `operator.toml`
  and its payload in `operator/`. `compat/hermes/` holds the compatibility
  lock, the PTY goldens and the evidence notes. `cargo xtask hermes
  generate-skill` writes the operator skill and its checksum into the package.
- Releases build `pohunek-runtime-hermes-<version>.tar.zst` like every other
  official package. `compat/matrix.json` has Hermes rows for
  `x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`, so the signed
  catalog authorizes the reserved `hermes` id on those targets and every Linux
  daemon archive ships `runtime/packages/hermes.tar.zst`. The macOS daemon
  archive carries no runtime package (#929). The release-evidence `rows` and
  `smoke` jobs install Python and uv for a provisioner lock, and the archive
  smoke restores every staged runtime from an untouched copy and verifies it
  before each Linux daemon archive.
- The built-in Hermes runtime and the CLI's compiled operator payload stay as
  the source for a host without an installed, enabled and selected official
  package.
- The regenerated operator skill changes the operator payload digest, so an
  existing operator install reports `outdated=true` in `integration status`
  until `pohunek integration update --agent hermes` for its Hermes profile
  rewrites it.
- The Linux CI job `hermes-package` follows the runtime package selection,
  builds its own binaries and the archive with `cargo xtask package build
  --evidence-dir`, matches a verified release baseline digest, runs the
  compatibility gate and the real-Hermes package test, and uploads
  `package-evidence-hermes` and `package-junit-hermes`. The macOS Hermes job
  also runs when the selection did not succeed or selects Hermes.
- Documented in the new `hermes-package` guide, the `hermes-operator` guide
  (regenerated Hermes skill), the `runtime-packages` guide,
  `concepts/release-bundle`, the index and the source map, and in
  `runtime-packages/README.md`, `compat/hermes/README.md`, `docs/install.md`,
  `docs/architecture.md`, `docs/development.md`,
  `docs/runbooks/hermes-operator-plugin.md` and
  `.agents/instructions/verification.md`.

### 2026-10-12, Hermes release consumer driver, #147

- The release consumer suite has a `hermes` driver. It installs the Hermes
  package from the catalog, runs `integration install --agent hermes` into a
  private Hermes home and requires the install and status results to report
  `provenance` `source=package` with the archive's package id and digest, then
  drives a real pinned Hermes through one turn against a loopback provider,
  stops it and resumes it by the conversation id the plugin hook reported. The
  Linux Hermes package CI job (`hermes-package`) runs it in row mode on the
  archive built from `runtime-packages/hermes`, after restoring and verifying a
  pristine stage. Documented
  in `docs/development.md` ("Release consumer suite") and the source map.

### 2026-10-12, provisioned upstream staging, #147

- `cargo xtask compat stage-upstream` stages a lock whose `upstream.kind` is
  `provisioner`, a Python virtual environment at `venv/`: it runs the script
  `upstream.provisioner` names (only
  `scripts/<name>`, a regular executable file, never a link) in place at
  `<out>/<runtime>` with a cleared environment, links `bin/<binary>` to the
  entrypoint the script prints, copies the lock and script into `inputs/`,
  requires the entrypoint's first line to be exactly
  `#!<stage>/venv/bin/python` or `#!<stage>/venv/bin/python3` (uv writes the
  latter on macOS),
  writes `STAGE.sha256` and `STAGE.links`, and requires the banner as the first
  line of `--version`. A stage path at which uv would write a `#!/bin/sh`
  trampoline is refused before the script runs. `compat verify-stage` also
  refuses a provisioned stage whose `inputs/` differ from the committed files,
  a trampoline entrypoint, another interpreter and a moved stage. Hermes is staged this way; its
  stage is never moved. Hermes writes into its editable checkout on launch,
  and a daemon-launched session and the CLI's `-I` plugin probes write
  bytecode into it, so a stage is verified from a pristine copy before each
  use. The pinned Hermes CI jobs stage through the command, cache a copy
  taken before any consumer runs, run `verify-stage` on a cache hit, and
  restore and verify that copy again before each later consumer. Documented in `concepts/upstream-staging`,
  `compat/hermes/README.md` and `docs/development.md`.

### 2026-10-12, release baseline in CI package selection, #149

- The `package-selection` CI job downloads release `v<workspace version>`'s
  runtime catalog, attestations and package archives with `gh release
  download` and runs `cargo xtask package baseline --json`. A pull request
  skips a package job only when the package and the core are unchanged and the
  signed release verifies the package; a failed download makes every package
  missing and runs every job, and only an unreadable checkout fails the
  selector. Each package job requires its rebuilt archive to hash to a verified
  baseline digest; a difference fails the job as a non-reproducible build.
  Release workflows never consult a baseline. Documented in
  `concepts/runtime-package-archive` ("Release baseline") and
  `docs/development.md`.

### 2026-10-12, runtime package builder evidence, #149

- `cargo xtask package build --evidence-dir <D>` writes the builder evidence of
  an official package next to its archive: `<archive>.sha256`,
  `<rt>.manifest.json`, `<rt>.provenance-input.json` (source commit and Git
  tree, builder, archive entries, the lock's whole `upstream` object of any
  kind) and `<rt>.compat-vector.json` (the package half of a compatibility
  attestation),
  as deterministic JSON. A package tree, lock or matrix that differs from
  `HEAD`, a missing or mismatched lock and an evidence directory inside the
  package are refused with nothing written. The per-PR package jobs upload it
  as `package-evidence-<rt>`, the release build as `package-evidence`; it is
  not a release asset. Documented in `concepts/runtime-package-archive` and
  `concepts/release-bundle`.

### 2026-10-12, runtime-declared transcripts, #487

- A runtime descriptor may declare `[transcripts]` (`parser`, a compiled parser
  id, and `dir`, a plain relative path below `[config_home]`); the external
  observer takes each transcript root and its parser from it instead of naming
  Claude and Codex. The built-in and official `codex` and `claude` descriptors
  declare `jsonl-head-v1` with `sessions` and `projects`, so the observed roots
  are unchanged; a runtime without the table (Pi) contributes no root, and a
  tree that equals or nests with another runtime's is attributed to neither
  (warning `external.transcript_roots.ambiguous`, code
  `transcripts_ambiguous_owner`). An
  unknown parser, an unsafe `dir` or a table without `[config_home]` is
  `descriptor_invalid`. Documented in the `runtime-packages` guide, the
  `sessions` and `agent-profiles` concepts and the public API.

### 2026-10-12, single worker-instance marker, #493

- `POHUNEK_WORKER_INSTANCE_ID` is the only worker-instance marker the process
  inspectors, the runtime sweep, the managed Claude/Codex hooks and the Hermes
  reporter read. A live worker whose journal names a release before 0.33.1 (or
  none) marks its children with another name, so the upgrade preflight reports
  it `would_not_be_adopted` with `worker_marker_legacy`, and startup leaves it
  running as `incompatible` with the same reason; `session stop`, input,
  resize and attach on it keep the code `worker_protocol_incompatible` but name
  `worker_marker_legacy` in `msg` with a `recover` hint. A sweep never
  signals a worker child (`POHUNEK_WORKER_ID`, set only by a worker) that
  carries the swept session's `POHUNEK_SESSION_ID` but no worker-instance
  marker; a lifecycle hook the daemon runs for the session has no worker
  marker and is ignored. Such a child keeps the cleanup unconfirmed
  (`runtime_lost_cleanup_unconfirmed`, removal refused) and names such
  processes by pid in the `session.runtime.sweep_unmarked` WARN. The runtime
  inventory lists a refused worker as `incompatible` with the same reason. Installed hooks of the previous release differ from the
  shipped assets and are reported for reinstall. Documented in
  `update-after-release`, `debug-session-runtime` and `concepts/sessions`.

### 2026-10-12, resumable Pi forks, #546

- A runtime with `[native_reference] strategy = "assigned"` may declare
  `assign_fork = true` (only next to `[fork] supported = true`, otherwise
  `descriptor_invalid`). `session.fork` then generates a fresh reference for the
  fork and appends the rendered `launch_args` after the `[fork]` args; the fork
  holds it with provenance `assigned` from creation, so it resumes into its own
  conversation after a stop or a daemon restart. The official Pi package
  declares it from package version 1.0.1 (`pi --fork <source> --session-id
  <fork>`, verified against Pi 1.0.2, which writes the fork's
  `<time>_<fork>.jsonl` at once); `plugin update` installs 1.0.1 beside 1.0.0. The flag is
  frozen into the session's native launch spec (store schema 6, a no-op
  migration from 5), and `PackageRuntimeInfo` gains `fork_launch_args`, shown by
  `plugin inspect` as the fork's `then adds:` line. Documented in
  `guides/pi-package`, `concepts/sessions`, `concepts/agent-profiles`,
  `docs/install.md`, and `docs/public-api.md`.

### 2026-10-12, Pi dialogs, selectors and session-dir overrides, #547

- The Pi package (version 1.0.2) reads Pi's dialogs as `blocked`: an
  extension's select, confirm, input or editor dialog (also a permission check
  raised mid-turn), the project trust prompt at startup, the missing session
  cwd prompt and every login step. A body that holds a selector list (a cursor
  row and a row indented by two spaces) withholds `idle`,
  so the model, session, tree, fork and settings selectors match no rule; a
  quoted draft (`> text`) stays `idle`. Pi's own confirms drawn like an
  extension dialog (project trust, missing session cwd, "Summarize branch?",
  "Import session"), the `/trust` selector and the login steps read `blocked`
  even when the owner opened them. `scripts/tests/test_pi_detect_regex.py`
  runs the generator in CI and requires `detect.toml` to hold its output. The
  1.0.1 manifest read 93 of 120 captured dialog screens and 36 of 50 selector
  screens as `idle`. Screens of a real Pi 1.0.2 at widths 20 to 200 are in
  `compat/pi/screens/overlays/`.
- A file existence check may declare `store_env` and `store_max_depth` (a
  variable that names the store itself, followed by the check),
  `relocating_args` and `relocating_settings` (store moves the check cannot
  follow). `session.new`, `session.resume` and `session.fork` refuse a launch
  that moves the store out of reach with the new code
  `runtime/agent_native_store_relocated` and a hint naming the variable. Pi
  declares `PI_CODING_AGENT_SESSION_DIR`, `--session-dir` and the `sessionDir`
  setting of the agent directory and the project. A settings path that is not
  a regular file (a FIFO, a directory) is refused without blocking. Recovery
  runs the same refusal, so a `sessionDir` written after launch, or the loss
  of the variable the launch had, refuses resume and fork; there the hint says
  to undo that change (remove the setting, or restore the variable to the path
  the session was launched with and accept the profile change). A crash of the
  check (on recovery the whole check, Claude's transcript check included) is
  `daemon/native_store_check_task_panicked`. The keys are frozen into the
  session's launch spec (store schema 7, a no-op migration from 6).
  Documented in the `pi-package` guide, `concepts/sessions`, the
  `debug-session-runtime` runbook, `docs/public-api.md`, `docs/install.md` and
  the source map.

### 2026-10-12, Claude package detection rules, #613

- The official Claude package is version 1.0.1. Its detection manifest adds to
  the built-in one: the `◐`/`◑` running-turn title and the `esc to interrupt`
  footer read `working`, so a streaming reply reads working; the question
  form reads `blocked` when its footer wraps at 30 and 40 columns; the theme
  picker, the login method list and the folder trust dialog read `blocked`;
  the completed-turn line with ` · done <time>` is the completed-turn rule; the
  npm `claude.exe` process matches, and a `node`/`bun` script that merely names
  `claude` (`claude-companion.mjs`) does not. The built-in manifest is
  unchanged. The package tests enumerate these differences from the v0.34.2
  golden. Documented in the `claude-package` guide and the source map.

### 2026-10-12, coalition exclusion in the runtime sweep, #821

- The worker journals its kernel coalitions as the optional `worker_coalitions`
  (`<resource>:<jetsam>`, decimal; additive, no journal schema bump, so a worker
  of the previous release lacks it). On macOS the sweep uses them for a hidden
  process whose fork lineage is open (a launchd creator number, or a creator
  that exited): when the kernel puts the process in a resource and a jetsam
  coalition that both differ from the worker's, it cannot have descended from
  the worker and is ignored, not reported. A launchd job, a system helper such
  as `mdworker_shared` or `biomesyncd`, or another session's agent started after
  the worker therefore no longer leaves a removal or a lost-runtime cleanup
  unconfirmed. Equal, unreadable or missing coalitions prove nothing and keep
  the process a candidate; a lineage descendant whose coalitions differ is
  skipped, never selected. Nothing is signalled that was not before.

### 2026-10-12, the sweep waits for unreadable candidates, #821

- The runtime sweep no longer leaves a cleanup unconfirmed because of a hidden
  process that is only passing: when a pass signals nothing and every remaining
  candidate is an unreadable-marker process, the daemon re-reads the process
  table for up to the sweep grace (`sweep.grace_ms`) before it reports those
  candidates. A process caught between images, or an orphan of a creator that
  has just exited and is about to follow it, no longer blocks a removal or a
  lost-runtime cleanup; a candidate that outlives the grace stays listed and the
  cleanup unconfirmed, exactly as before.
- The wait is shared by every runtime one operation sweeps: the startup
  reconciliation, one background re-check of a session, and one removal or stop
  cleanup each wait at most one sweep grace in total for such candidates, so
  that wait adds at most one grace to startup readiness however many crashed
  runtimes a persistent candidate blocks; the runtimes decided after it stay
  lost with their cleanup unconfirmed.

### 2026-10-12, CLI session turn waits, #858

- `pohunek session wait <target>...` waits for turn events through
  `session.wait_turn` when no legacy flag is given, also with no flags at all:
  `--until` (comma list or repeated, default all of `turn_completed`,
  `agent_blocked`, `approval_required`, `exited`), `--after <cursor>`, and
  `--timeout <duration>` with a unit (`90s`, `5m`; default the daemon default).
  It takes up to 32 targets on one host; targets on different hosts, several
  targets in legacy mode, and legacy flags mixed with turn flags are usage
  errors before any request. `--json` prints the daemon result; the human form
  prints the event or reason, one `progress:` line per target, and a final
  `cursor:` line. Every reason exits 0. The legacy predicate wait is selected by
  any of its flags and is unchanged. Documented in `docs/cli.md`,
  `concepts/sessions`, the `debug-session-runtime` runbook, a manager-agent loop
  in the `agent-skill` guide (regenerated skill artifact), and a note in the
  `hermes-operator` guide that the plugin's wait tool is the short predicate
  wait (regenerated Hermes skill).

### 2026-10-12, notifications watch filters, #858

- `pohunek notifications watch` accepts `--session <id>`, `--kind <kind>`, and
  `--status <status>` (`--unread` is `--status unread`), with the values of the
  matching `notifications list` flags. The daemon subscription stays global;
  the filters run client-side on the record each `notification_created` or
  `notification_updated` event carries, on every host with `--all-hosts`, in
  human and JSON output. An update that marks an earlier `turn_completed`
  `acknowledged` therefore does not pass `--unread`. While any filter is set,
  `notification_deleted` events are dropped, because they carry only the
  notification id, and `--status deleted` is refused. Without filters every
  event is printed. Documented in the
  `debug-daemon` runbook, the `remote-hosts` guide, `docs/cli.md`, and
  `docs/public-api.md`.

### 2026-10-12, service.toml turn-wait timings, #858

- `service.toml` schema 4 adds `[waits]`: `turn_default_ms` (the timeout of a
  `session.wait_turn` call that names none, 300000 on fresh installs) and
  `turn_max_ms` (the ceiling a requested timeout may not exceed, 1800000).
  Both are whole milliseconds in `5000..=3600000`, and a default above the
  ceiling makes the daemon refuse startup. `pohunek service upgrade` migrates
  schema-2 and schema-3 files with the same backup and rollback transaction,
  assigning the defaults of the missing tables and keeping every recorded
  value; `config_schema_migration` reports `2` or `3`. Documented in
  `guides/setup`, the `update-after-release` runbook and `docs/public-api.md`.

### 2026-10-12, session turn waits, #858

- New protocol method `session.wait_turn` (Rust SDK `Client::session_wait_turn`)
  waits for the next `turn_completed`, `agent_blocked`, `approval_required`, or
  `exited` event of one or more sessions after an opaque cursor, bounded by an
  agent-chosen timeout (daemon default 5 minutes, ceiling 30 minutes). Results
  carry the next cursor and per-session progress; reasons are `event`,
  `timeout`, `cursor_reset`, and `targets_ended`. Turn events come only from
  admitted provider hook reports for held sessions and from the daemon
  projector, and are recorded after the attention debounce whether or not the
  notification policy keeps an inbox record; a `turn_completed` whose window
  ends while the session works is dropped as a stale stop. `host.inspect` advertises the method with
  `session_wait_turn_supported`. Documented in `concepts/sessions`, the `debug-session-runtime`
  runbook, and `docs/public-api.md`.

### 2026-10-12, TypeScript turn waits, #858

- The TypeScript SDK adds `client.sessionWaitTurn(params)` for
  `session.wait_turn`, on a dedicated connection whose deadline follows
  `timeout_ms` or, when it is omitted, `MAX_SESSION_WAIT_TURN_MS`; generic
  `call("session.wait_turn", ...)` takes the same path. `@pohunek/protocol`
  exports `MAX_SESSION_WAIT_TURN_MS`, `MAX_SESSION_WAIT_TURN_TARGETS`, and
  `MAX_SESSION_TURN_CURSOR_BYTES`. The testkit fixture daemon serves the method
  and adds `scenario.recordTurnEvent`. Documented in the `ts-sdk` guide,
  `concepts/sessions`, `sdk/ts/sdk/README.md`, and `docs/public-api.md`.

### 2026-10-12, subagent restart, Claude helper stop, main-turn snapshot and hook failure trace, #883

- A `SubagentStart` with a newer sequence for a terminal subagent reopens its
  record: `running` with a new revision, `finished_at_ms` cleared,
  `started_at_ms` kept; the next `SubagentStop` completes it. This tracks
  Claude background agents resumed with `SendMessage` or woken by their own
  background shell. A start for a running subagent is idempotent.
- A subagent record's phase is the phase implied by the highest-sequence
  claim the worker admitted for its id, for every provider. A newer start for
  a running record or a newer stop for a terminal one is ignored but its
  sequence is persisted in the journal, so a delayed older claim cannot reopen
  or complete the record; a repeated Codex per-turn stop on a finished child
  only advances that sequence.
- `identity-subagent-v1` lists `claude` as a provider whose stop is admitted
  only after a recorded start. Claude's prompt-suggestion helper stop after
  every main turn creates no row, changes no count and evicts no history. The
  worker remembers its sequence in an in-memory set of at most 64 stops, drops
  an entry once a start for that id applies, and ignores (`stale_sequence`) a
  delayed start that is not newer than the remembered stop. A Codex stop
  without a start records a completed subagent (#916).
- The worker logs every ignored subagent claim as
  `worker.subagent.claim.ignored` (`duplicate_start`, `stop_without_start`,
  `stale_sequence`, `not_running`; INFO, hook answer `ok: true`) and every
  refused one as `worker.subagent.claim.refused` (`capacity`; WARN, hook
  answer `ok: false`).
- The Claude state hook runs on the main-agent `Stop` (a second managed `Stop`
  entry beside the notify hook) and sends the worker a `subagent_snapshot`
  claim with only the `id`, `type` and `status` of each `background_tasks`
  entry. It reads at most 1 MiB for this action (64 KiB for the others) and
  sends nothing for a `Stop` with a non-empty `agent_id` or a missing, invalid
  or over-256-entry list. `identity-subagent-v1` admits the snapshot.
- The worker reconciles the rows of the same reporting process with an older
  sequence and settles only rows whose end the snapshot proves: a running row
  missing from the snapshot whose newest listing by the reporter falls within
  its run and is older than the snapshot is `completed`, and any other missing
  running row is `completed` only when no task that can host an unlisted agent
  (any type except `shell`, `monitor` and `MCP task`) is listed and the
  reporter's newest snapshot is older than this one and newer than the row,
  because Claude builds the listing before the hook reads its clock. Only that
  newest evidence is kept, so a snapshot older than it, delivered out of
  order, settles nothing from it. A listed row,
  running or terminal, is left unchanged, so a fast agent's own stop completes
  it and a self-woken agent reopens through its own start; a completed row keeps its
  own sequence, so a resume start ordered before the snapshot still reopens
  it. It logs `worker.subagent.reconciled` (`snapshot_absent`) and
  `worker.subagent.snapshot.applied` with the task count `tasks`, the `hosts`
  flag and the row counts `listed`, `terminalized` and `deferred`. A
  never-listed row settles at the earliest at the second main-turn end after
  its start; a row whose reporter never ends another main turn stays
  `running` until the runtime ends (`lost`). Codex has no snapshot (#916).
- The sequence contract names the clock the Codex and Claude hooks read:
  `time.clock_gettime_ns(time.CLOCK_MONOTONIC)`, system-wide, because
  `time.monotonic_ns()` is defined only within one process.
- Once a Codex or Claude state hook has seen `POHUNEK_ENV=1` and a session
  id and started `python3`, it records every reason it fails to deliver a
  report as one line in the hook failure trace `pohunek-hooks.jsonl` in the
  pohunek log directory: `input_too_large`, `input_invalid`, `no_snapshot` (with
  `detail` `subagent_stop`, `tasks_missing`, `tasks_invalid` or
  `tasks_over_bound`), `worker_socket_missing`,
  `worker_instance_missing`, `agent_pid_invalid`, `agent_identity_unavailable`,
  `worker_unreachable`, `worker_timeout`, `worker_closed`, `worker_refused`,
  and `daemon_socket_missing`, `daemon_unreachable`, `daemon_timeout`,
  `daemon_closed`, `daemon_refused` and `daemon_protocol_invalid` for the
  daemon fallback. A line is one JSON object of at most 512 bytes with `ts`,
  `session`, `provider`, `action`, `reason` and an optional fixed `detail`
  code, and never carries payload content, input ids, task data, paths or
  environment values. The worker injects the reserved `POHUNEK_HOOK_TRACE_PATH`
  and `POHUNEK_HOOK_TRACE_MAX_BYTES` (1 MiB); the hook traces only when both
  are valid, writes its lines at exit under an exclusive lock that it tries
  without blocking (at most 5 attempts, 5 ms apart, else the run's lines are
  dropped), opens the file
  with `O_NOFOLLOW` and mode `0600`, writes only to an owner-private regular
  file of the user with one link, and rotates it once into
  `pohunek-hooks.jsonl.1`, so the trace holds at most 2 MiB. Tracing never
  changes the hook's exit code or output, a report delivered on its first
  route writes no line, and a hook without `python3` writes no trace.
- `POHUNEK_INTEGRATION_VERSION` is 13. `SubagentInfo` and the public wire
  protocol are unchanged; the private worker hook protocol carries
  `subagent_snapshot`. Documented in `concepts/sessions`, the `claude-package`
  and `codex-package` guides, the `debug-session-runtime` and
  `update-after-release` runbooks, the `runtime-packages` guide,
  `docs/architecture.md` and
  `docs/public-api.md` (hook schemas, `SessionInfo.subagents`,
  `subagent_state`, Provider Notification Hooks, Hook failure trace,
  Active-Agent Hook Payloads reserved environment, Claude notification hooks,
  integration version).

### 2026-10-12, background work snapshot in the Claude hook, #890

- `POHUNEK_INTEGRATION_VERSION` is 14. The Claude `stop` hook forwards each
  background task's sanitized `label` (every type), `program` (`shell` only,
  the command word's basename) and `agent_type` (`subagent` only) in the
  `subagent_snapshot` claim. The command and its arguments are never forwarded;
  the label and program limits come from worker-injected environment variables.
  The `session` hook forwards `session_source` (`startup`, `resume`, `clear`,
  `compact`) in its identity report.
- Documented in the `claude-package` guide. Concept and CLI pages follow with
  the later slices of #890.

### 2026-10-10, macOS 27 withheld environments, #790

- On macOS 27 the kernel withholds the environment of restricted Apple platform
  binaries (`/bin/sh`, `/bin/zsh`, `/bin/sleep`). The Darwin inspector now reads
  such a process as unobservable instead of unmarked, so a lost-runtime sweep
  lists every one of them as an unreadable candidate and the cleanup stays
  unconfirmed (`session rm` refuses until they are inspected or
  `--accept-unconfirmed-cleanup` is given). The runbook `debug-session-runtime`
  names the case.

### 2026-10-10, fork lineage decides withheld environments, #795

- The runtime sweep decides macOS 27 platform binaries by fork lineage instead
  of listing each as an unreadable candidate. The kernel serves a boot-wide
  creation number for every same-user process, withheld environment or not, and
  records the creation number of the creator. That creator number is rewritten
  to launchd's when the process re-executes after being reparented, so a
  launchd creator is ambiguous and never excludes a process. The worker journals
  its own number as the optional `worker_spawn_id` (decimal string, no schema
  bump), and the daemon gives it to every journal-backed sweep only when the
  journal's boot identity is the current boot's. A process the worker reached
  through its creators is reaped like a marked one; a process created before the
  worker, or under an older non-launchd creator, is ignored and not reported;
  every other hidden process (created after the worker by launchd, a re-executed
  orphan, one whose creator exited after the worker started) stays an
  unreadable candidate and the cleanup unconfirmed. After a lost runtime on
  macOS 27 the runtime's own processes are reaped without
  `--accept-unconfirmed-cleanup`; the flag may still be needed for the
  remaining candidates, which the refusal lists by pid and command name, and
  for a runtime whose worker journaled no creation number (started by the
  previous release). Linux keeps its start-time ordering. Documented in the
  `debug-session-runtime` runbook, `concepts/sessions`, `concepts/architecture`
  and `docs/cli.md`.

## 0.34.0

### 2026-10-08, digest-bound release evidence, #150

- The Release workflow is rewired: producers upload workflow artifacts only,
  the reusable `release-build.yml` and `release-evidence.yml` build the
  packages and run the compatibility rows, provenance, catalog signing and the
  archive smoke, and a single `publish` job creates a draft release, uploads
  and verifies the complete inventory and then publishes it. `ci.yml` rehearses
  the same flow on pull requests with a throwaway key.
- `packaging/smoke-archive` runs a finished daemon archive in a network- and
  PID-isolated namespace: it verifies the staged upstreams, installs every
  shipped package from the archive's own signed catalog and anchor, and runs
  the real-runtime consumer suite against the archive's binaries.
- `cargo xtask compat stage-upstream|verify-stage` install and verify the pinned
  upstream runtime releases from `compat/<rt>/npm` lockfiles with an integrity
  per package; the locks gain `integrity`, `scripts` and `version_output`. The
  concept page `upstream-staging` describes the stage layout and its POSIX
  verifier.
- `cargo xtask release assemble|verify-inventory` builds and checks the release
  bundle from the producer artifacts: it recomputes every digest, verifies the
  attestation of each matrix row, signs the schema 2 catalog and rebuilds the
  daemon archives with a `runtime/` directory. `packaging/release-policy.json`
  is the single source of the expected inventory; the concept page
  `release-bundle` describes the layout, the macOS-without-packages rule and
  the inventory the publisher verifies.
- Runtime catalog schema 2: a signed `release` block (version, commit, binary
  sets per target) and, per entry, `runtime_api` and per-platform attestation
  digests; the signing domain separator is v2, schema 1 documents are refused.
  The daemon does not enforce the new fields yet.
- `cargo xtask compat attest|verify|matrix-check`, the canonical binary-set
  digest and `compat/matrix.json` (runtime x target rows, suite version)
  define the compatibility attestation each release row emits; the new concept
  page `release-attestation` describes the tuple and the trust model.
- The release consumer suite (`crates/cli/tests/release_consumer.rs`) runs one
  runtime out of process through the exact release binaries and writes the
  consumer report that `compat attest` consumes.

### 2026-10-08, digest-bound release evidence, #150

- Runtime catalog schema 2: a signed `release` block (version, commit, binary
  sets per target) and, per entry, `runtime_api` and per-platform attestation
  digests; the signing domain separator is v2, schema 1 documents are refused.
  The daemon does not enforce the new fields yet.
- `cargo xtask compat attest|verify|matrix-check`, the canonical binary-set
  digest and `compat/matrix.json` (runtime x target rows, suite version)
  define the compatibility attestation each release row emits; the new concept
  page `release-attestation` describes the tuple and the trust model.

### 2026-10-08, digest-bound release evidence, #150

- Runtime catalog schema 2: a signed `release` block (version, commit, binary
  sets per target) and, per entry, `runtime_api` and per-platform attestation
  digests; the signing domain separator is v2, schema 1 documents are refused.
  The daemon does not enforce the new fields yet.

### 2026-10-08, production catalog trust anchor is checked in

- Decision: the release trust anchor `runtime-catalog-anchor.json` has two
  roots, both valid from 2026-10-08T00:00:00Z: the CI secondary root
  (`e68204fe...cd771`, until 2028-10-08) signs routine release catalogs and the
  offline primary root (`f52cf66e...989fe`, until 2036-10-08) signs only CI-key
  endorsement and CI-revoking catalogs. The CI root is rotated before its
  `not_after` by a primary-endorsed `key_chain` record and a new anchor in the
  next release.
- The finished anchor is checked in at `packaging/runtime-catalog-anchor.json`
  from public key files under `packaging/catalog-trust/`, with a test that
  regenerates it through `catalog anchor` and requires byte equality, so the
  shipped file is reproducible from reviewed public inputs. The runtime catalog
  concept page names the roots, windows and rotation rule (#655).
- Daemon release archives carry the anchor at the archive root beside `pohunekd`;
  CLI and relay archives carry none, and staging a daemon archive without the
  checked-in anchor is refused.
- The installer copies the anchor into the version directory beside the daemon, so
  the `catalog_trust_anchor` check is `ok` after a release install; an invalid
  staged anchor fails the install before anything is published, and a different
  anchor on republish is a version conflict.

### 2026-10-08, catalog anchor writes several roots

- `cargo xtask catalog anchor` takes a repeatable `--root
  <public-key-file>:<not-before>:<not-after>` instead of the single-key
  `--public-key-file/--not-before/--not-after` flags, so one anchor lists the
  offline primary root and the CI secondary root (owner decision on #150), each
  with its own window, plus the revoked key ids. Root count, repeated keys and
  empty windows are refused by the anchor validation with no file written; the
  output is sorted by key id. The runtime catalog concept page states the form
  and the custody roles (#650).

### 2026-10-07, stop and remove need no runtime definition

- `session.stop` and `session.remove` no longer require the session's agent
  runtime to resolve, so the owner can end the worker of and delete a session
  whose runtime is not installed or whose recorded kind is a historical label.
  Resume, fork, input and every other mutation stay refused with
  `runtime_not_installed` or `agent_kind_unsupported`; external sessions stay
  read-only. The public API reference and the agent-profiles concept page state
  the rule.

### 2026-10-07, outdated integration hook assets are flagged after an upgrade

- Decision: after a daemon upgrade the managed hook assets are flagged loudly and
  reinstalled by one documented owner command; the daemon and the installer never
  rewrite files in an agent's config home on their own, because those homes are
  user-owned and an install also edits the provider's registration files.
- Claude and Codex status already compared the full installed script with the
  embedded one, so content drift at an unchanged version marker was reported as
  outdated; the doctor classifies it as `asset_modified`. Hermes compared the
  installed plugin only with the checksums its own ownership marker recorded, so
  an untouched install from an older release passed. Hermes status now reports
  `outdated`, and the Hermes doctor has an `asset_current` check (16 checks).
- A launch (new, resume or fork) whose agent has outdated Claude or Codex hook
  assets carries a session warning of the existing `hook` kind naming the install
  command. Decision: no new `SessionWarningKind`, because warnings are persisted
  in the session record and decoded as a closed enum by released clients and by
  an older daemon reading the store, so a new variant would need a protocol and
  store schema bump with an adapter; `hook` already means a hook that could not
  do its job. The warning is recomputed at each launch, so it stays current. The installer wrapper prints the doctor and
  install commands after a successful install or upgrade, and the update runbook
  has a "Managed hook assets after an upgrade" section.

### 2026-10-07, package test daemons use the test-host process view

- The shared package test harness builds its daemon with
  `SessionRegistry::new_production_with_inspector` and the test-host process
  view, so an unreadable same-user process of a loaded test host no longer makes
  a session removal refuse. Production daemons keep the real host view.

### 2026-10-07, Claude hook reporter behind a wrapper shell

- The managed Claude hook scripts skip a wrapper shell when they pick the process
  that reports: Claude runs hooks as `/bin/sh -c`, and where that is dash the
  shell stays between Claude and the script, so the worker rejected the
  SessionStart report and no native reference was recorded. The Claude package
  guide records the finding.

### 2026-10-07, Codex conversation id becomes the native reference

- The session worker verifies a launch claim from a provider-named direct child
  of the launch process in the provider's hook-helper role (`codex app-server`), so the conversation id Codex reports from its
  `app-server` child becomes the session's native reference and `session resume`
  works. The sessions concept page states which process may report the id, and
  the Codex package guide replaces its known-gap statement with the verified
  behavior.

### 2026-10-07, Claude Code runtime package source

- Added the Claude Code runtime package guide and the package source
  (`runtime-packages/claude`, `compat/claude/compatibility-lock.json`,
  `compat/claude/screens/`): the descriptor and detection manifest equal the
  built-in Claude files, plus a `semver-line-v1` probe for `<release> (Claude
  Code)` from 2.1.289. The package can be served as the official `claude` runtime
  only through a signed catalog, no release catalog or signing key exists yet,
  and the built-in Claude runtime stays in the daemon. The guide records the
  first-run state files of a Claude home, what was verified against a real
  Claude Code 2.1.289 (hooks from the launched process, resume and fork argv,
  subagent hooks) and the gaps found: the working title uses a glyph no rule
  names, narrow question forms and the trust dialog are not classified, and a
  forked session keeps its source's native reference so its own conversation id
  is rejected.
- The source map lists the package source, the lock, the screens, the tests, the
  Messages stub, the shared process guard and the `claude-package` CI job.

### 2026-10-06, Codex hook reporter ancestry

- The real-Codex hook test requires the reporting process to descend from the
  launched process instead of being its direct child, because an npm install
  launches Node, then the native binary, then the app-server. The guide records
  the observed process tree of both installs and which processes the package
  matchers accept.

### 2026-10-06, Codex package tests install the package

- The Codex package tests install the built archive through a signed catalog
  with a throwaway key and trust anchor, so the real-Codex tests launch through
  the installed package and its version probe, and an always-running test drives
  that probe with unsupported and supported banners. The fixture kills every
  process it started (worker, Codex, detached app-server) on success and on
  failure. The guide and the package README state that the package is official
  only through a signed catalog and that no release catalog or key exists.

### 2026-10-06, trust anchor ACL policy

- The catalog trust anchor and the catalog signing key file are judged on macOS
  extended ACLs as well as mode bits, through the shared
  `pohunek_platform::filesystem::acl_grants_change` (anchor: no allow entry that
  lets another principal change the file) and `acl_grants_access` (signing key:
  deny-only ACLs only). An unreadable ACL fails closed. Documented in the
  runtime catalog concept.

### 2026-10-05, Codex runtime package source

- Added the Codex runtime package guide and the package source
  (`runtime-packages/codex`, `compat/codex/compatibility-lock.json`,
  `compat/codex/screens/`): the descriptor and detection manifest equal the
  built-in Codex files, plus a `semver-line-v1` probe for `codex-cli 0.160.x`.
  The package can be served as the official `codex` runtime only through a signed
  catalog, and no release catalog or signing key exists yet. The guide
  records what was verified against a real Codex 0.160.0 and two gaps: the real
  folder-trust dialog wording is not matched by `workspace_trust_prompt`, and
  Codex runs its hooks from an app-server process below the launched one, so the
  hook-reported conversation id is not recorded as the native reference.
- The source map lists the package source, the lock, the screens, the tests, the
  Responses stub and the `codex-package` CI job.

### 2026-10-05, reporter templates

- The Codex reporter scripts are core-owned templates with `@POHUNEK_AGENT_ID@`
  and `@POHUNEK_AGENT_NAME@` placeholders, rendered from the runtime's id and
  display name before staging. Rendering refuses a value outside the safe
  character set. The installed bytes are unchanged, so trust hashes and
  installed integrations are unaffected. Documented in the runtime packages
  guide and the source map.

### 2026-10-05, catalog trust anchor

- The daemon reads its catalog trust anchor from `runtime-catalog-anchor.json`
  beside its executable (public keys, windows, revoked key ids), with an
  integrity policy on the file and its directories. A missing file keeps
  `official_trust_unavailable`; a file or location that cannot be trusted gives
  the new `official_trust_anchor_invalid` and a failing `catalog_trust_anchor`
  doctor check, while the daemon keeps serving local trust. `cargo xtask catalog
  build|sign|verify|anchor|public-key` builds, signs (with an owner-private key
  file named by path) and verifies catalogs and writes anchor files. No key is
  shipped or compiled in; custody and release staging stay with #150. Documented
  in the runtime catalog concept, the runtime packages guide, the public API
  reference and the source map.

### 2026-10-05, line version probe

- Runtime descriptors can declare `version_probe = { parser = "semver-line-v1",
  args, min, below, line }`, where `line` is a literal template
  (`codex-cli {version}`, `{version} (Claude Code)`, `Hermes Agent v{version}
  {annotation}`) the first output line must match, so the official runtimes'
  real `--version` banners are readable by a package descriptor. Pre-release
  suffixes never match. Templates are validated with the package. `semver-v1`
  is unchanged. Documented in the public API reference, the agent profiles
  concept and the source map.

### 2026-10-05, official runtime aliases

- A catalog-authorized official package may serve `codex`, `claude` or `hermes`
  in place of the built-in runtime; local, linked and explicit-digest packages
  and the shell stay refused. Disabling or uninstalling the package returns the
  alias to the built-in. A session launched from the built-in runtime is refused
  resume and fork with the new `runtime/runtime_served_by_package` error while a
  package serves its runtime, keeping its binding. The runtime packages guide,
  runtime catalog and archive concepts and the public API describe it.

### 2026-10-05, preflight hook schema

- The upgrade preflight resolves the hook schema of a worker whose journal
  carries none from the runtime definitions (built-in runtimes and the package
  store, read-only), so a live agent session that reported its active identity
  is adoptable. `worker_identity_unverified` remains only for a runtime whose
  definition cannot be read.

### 2026-10-05, config-home id and observer roots

- `host.inspect` runtime entries may carry `config_home_id`, an opaque keyed
  identifier of the config home a launch of the entry gives its agent (bare
  runtimes that declare `[config_home]` and each profile), so clients group
  profiles by account without learning a path. External observation now watches
  the transcript tree of every distinct config home of the host, re-read on every
  pass, instead of one root per provider taken from the daemon's own environment.
  The agent profiles, sessions and secrets pages and the public API describe it.

### 2026-10-05, config homes

- A runtime descriptor may declare its config home in `[config_home]` (a
  variable name and a home-relative default); the built-in `claude` and `codex`
  declare `CLAUDE_CONFIG_DIR` / `.claude` and `CODEX_HOME` / `.codex`, moved out
  of code. One resolver gives the directory a launch would give the agent: the
  variable from the daemon's base environment overridden by the profile's
  `[env]`, else the default below `HOME`. A value that is not absolute is
  refused, never expanded. The daemon's own process environment no longer steers
  install or status, so they agree with the launched agent.
- `integration.install`, `status`, `doctor` and `uninstall` accept `profile` and
  `all_profiles` (local control socket only), report the `home` of each result,
  run one transaction per distinct home and list a failing home in `failed`.
  `pohunek integration` gains `--profile` and `--all-profiles`. The agent
  profiles page starts the subscription-switching guide, the public API, CLI,
  runtime packages and secrets pages describe the surface.

### 2026-10-05, upgrade test

- CI upgrades a session started on the previous release's published binaries
  to the current build through `pohunek service check` and `service upgrade`
  and asserts the session survives (`scripts/upgrade-test`, the `upgrade`
  workflow, and `release.yml` before any publishable binary is built). The
  update-after-release runbook and the source map describe it.

### 2026-10-05, frozen profile revision

- A session launched from a host profile freezes the profile's keyed revision
  into its resume binding (store schema 3, optional `profile_revision`).
  `session.resume` and `session.fork` resolve the profile once and relaunch only
  while the revision matches: an edited profile or a legacy session fails with
  `agent_profile_changed`, a missing one with `agent_profile_missing`, and an
  unreadable key with `agent_profile_revision_unavailable`. The owner's
  `accept_profile_change` (`--accept-profile-change`) relaunches under the
  current profile and re-freezes it; `session.resume` still accepts a bare
  session id, and the decision is honored on the local control socket only
  (`agent_profile_change_local_only`). The profile environment now travels in a
  redacting `LaunchEnv` carrier. The public API, CLI, architecture, secrets and
  agent profile docs describe the behavior.

### 2026-10-05, hook schema gaps

- A hook schema now declares the subagent sequence rule next to the subagent
  fields, and a schema without the rule admits no subagent record. The daemon
  import applies the schema's ancestry matcher to identities and its field and
  outcome set to subagents. The integration update check derives the operations
  of the staged asset set from the staged registration and the embedded scripts'
  action tables. The public API and the sessions and runtime packages guides
  describe the rules.

### 2026-10-05, compatibility-gated selection

- A package version whose integration (handler id and hook schema id, or none)
  differs from a version of the same package that a live, lost or resumable
  session or a host profile pin still references stays installed but unselected:
  `package.select` is refused with `package_integration_incompatible`,
  `package.install` with `select: true` installs it unselected, and
  `package.list`, `inspect` and `doctor` report `selection_blocked`. The state
  is derived from the registry and the retention scan, clears when the last
  reference is gone, and never selects the package on its own. The runtime
  packages guide and the public API describe the rule, `PackageRuntimeInfo`
  gained `hook_schema`, a profile pin of an incompatible version is refused, a
  launch that raced a select gets `runtime_package_changed`, and the
  integration update path checks the asset set against the schemas of retained
  versions as well.

### 2026-10-05, integration handlers

- Integration install, status, doctor, and uninstall dispatch through the
  compiled handler a runtime definition names (`integration.handler`) instead of
  the runtime id. The runtime packages guide and the public API describe the
  closed handler set, the staged update transaction (stage, compatibility
  check, atomic activation, rollback to the exact prior tree), the
  `integration_update_incompatible` error, and that `hermes-hook-v1` is
  registered with its lifecycle in the CLI. The source map lists the handler
  sources. `pohunek integration --agent` accepts any
  runtime id (built-in names unchanged), and status and doctor recovery commands
  name the runtime that was addressed.

### 2026-10-05, upgrade preflight

- The update runbook describes the read-only upgrade preflight that
  `pohunek service check|upgrade` run with the new binaries: the store dry run,
  the per-session verdicts (`adoptable`, `would_lose_recovery`,
  `would_not_be_adopted`) and their reason codes, what it cannot see, the error
  codes `service_upgrade_sessions_at_risk`, `service_upgrade_store_unusable` and
  `service_upgrade_preflight_failed`, and that `--accept-runtime-loss` is the
  one consent flag shared with the legacy migration path.
- The CLI reference, the setup guide, the macOS runbook, the session-runtime
  runbook and the public API cross-reference it.

### 2026-10-05, hook schemas

- The hook schema is enforced on the public socket (`session.report_agent`,
  `session.release_agent`, `session.report_native_id`, hook-claim
  `notification.create`) as well as on the worker socket.

- Runtime descriptors name a core-owned hook schema next to the integration
  handler (`[integration] handler`, `hook_schema`). The runtime packages guide
  and the public API document the closed schema set (`identity-v1`,
  `identity-subagent-v1`), the install-time refusals, and that a runtime without
  an integration accepts no hook report. The sessions concept describes how the
  schema reaches the worker, is journaled, and validates daemon imports,
  including the re-projection of a worker that journals no schema.

### 2026-10-05, Pi detection and probe PATH

- The Pi package guide describes detection as one bottom-anchored editor frame
  (upper border, draft, lower border, footer) read for both states, with busy
  borders recognised at every terminal width (their closing rule can be one
  column), the busy indicators (`Working`, compaction, retry), the 20-column
  floor for idle, the rule that draft and transcript text never decides, and
  that the working and idle rules cannot both match. It
  also states that the version probe runs under the launch `PATH`, profile
  override included.

### 2026-10-05, client-side protocol window

- The Rust client and the CLI advertise the protocol window `3..=4`
  (`CLIENT_PROTOCOL_VERSIONS` is `SUPPORTED_PROTOCOL_VERSIONS`) and translate
  through the `v3` adapter against a daemon of the previous release, so a
  staggered multi-host upgrade no longer needs one pass for them. A method new
  in the current protocol fails on the client with `daemon/daemon_protocol_too_old`
  (host, daemon protocol, required protocol, upgrade hint); an untranslatable
  payload fails with `daemon/version_translation_failed`. Discovery classifies
  peers for the asking client's range. The TypeScript SDK advertises the same window
  and carries a port of the adapter (`sdk/ts/sdk/src/compat.ts`) with a typed
  `daemonProtocolTooOld` error; its key names and introduced-method list are
  generated from the Rust adapter. The update runbook gains "Mixed releases
  across hosts"; `public-api.md` and the architecture "Protocol window" section
  describe the client side. The source map lists the client fixture test.

### 2026-10-05, reported references supersede assigned ones

- A validated report replaces an assigned native reference: the sessions page,
  the public API reference and the runtime packages guide state that an
  `assigned` runtime with an integration handler and hook schema follows
  `/clear` and in-session resume, that the replacement sets the provenance to
  `reported` in the session, its record and its resume binding, that resume and
  fork then use the reported reference without the existence check, and that an
  assigned value never replaces a reported one. Report sequences are ordered
  per transport (worker claim or public report) and per runtime generation, the
  worker's verified launch process (not only the root child) is the process
  whose conversation switches count, and the session store schema is 4. The
  worker journals the launch process's latest reference apart from the claim
  lease, every replacement carries an ordering key, and a recovered session
  keeps the key of its record.

### 2026-10-04, upgrade window and store schema

- The update runbook states the upgrade window (release N carries N-1 for live
  workers, the worker journal and public-protocol clients; persisted state
  migrates from any older kept schema), the `metadata.jsonl.pre-schema-<old>`
  backup, and the recovery from a newer-schema refusal. The daemon debug
  runbook points at it, and the source map lists the schema and shape-guard
  sources.

### 2026-10-04, conflict watch and stop exit

- Documented the WARN logged for every `conflict`, `lost`, `reconnecting`, or
  `incompatible` classification, the background re-check of a `conflict` and
  what ends it, adoption of a live worker after an upgrade (a binding without a
  launch spec does not block it), and `session stop` of a `conflict` with its
  journal and job proof and error codes.

### 2026-10-04, public protocol window

- Documented the public protocol window: a daemon accepts protocol `3..=4`
  through shape-only edge adapters while clients advertise only their own
  version, `daemon.health` and `host.inspect` report the negotiated version, and
  the update runbook now says to upgrade daemons before their clients (a new
  client against an N-1 daemon remains #527). The source
  map lists `crates/protocol/src/compat/`.

### 2026-10-04, Pi runtime package

- Added the Pi runtime package guide: package source layout
  (`runtime-packages/pi`, `compat/pi`), explicit-digest install, the assigned
  `--session-id`/`--session`/`--fork` contract, the session-file existence check
  verified against Pi 1.0.2, detection facts, limits, and how CI keeps the
  descriptor, the supported range and the tests in sync.
- The source map lists the package sources, the compatibility lock, the real-Pi
  test and the `pi-package` CI job.

### 2026-10-04, data-driven version probe

- Runtime descriptors can declare `version_probe = { parser = "semver-v1", args,
  min, below }`: the daemon probes the program with the declared argv in the
  sandboxed probe environment and accepts a release in `[min, below)`, so a
  package moves its supported range without a daemon release. Documented in the
  public API reference and the source map.

### 2026-10-04, legacy resume binding migration

- Documented that the store migration maps v0.33.0 flat recovery fields and
  repairs v0.33.1 records that lost their native launch spec, and that an
  unrepairable record logs a WARN, carries a `native_recovery` session warning
  and is never resumed automatically. The public API lists the new warning kind.

### 2026-10-04, README and reference pages

- The README is an agent-focused entry point; the human reference lives in
  `docs/features.md`, `docs/install.md`, `docs/cli.md`, `docs/sdk.md`, and
  `docs/development.md`, which every release archive carries next to the
  README. The source map lists those pages, and the macOS install runbook names
  them among the archive contents.

### 2026-10-04, daemon package host

- Documented the daemon side of runtime packages: the `<state>/plugins` store,
  `PackageSource` loading, explicit reload, pin resolution by digest,
  verification before launch and integration changes, the
  `runtime_incompatible` error and retention-checked uninstall.

### 2026-10-04, runtime catalog

- Added the runtime catalog concept: the signed catalog that authorizes official
  runtime packages, canonical signed bytes, key rotation and revocation,
  anti-rollback, reserved runtime ids, and explicit-digest local trust. The
  source map lists the new `crates/package` files.

### 2026-10-04, runtime package archive

- Added the runtime package archive concept: the canonical deterministic
  `tar.zst` format, the strict reader's rejection rules, the named size limits,
  and `cargo xtask package build|verify`. The source map lists the new
  `crates/package` files.

### 2026-10-04, assigned native references

- Runtime definitions declare how core obtains the native session reference in
  a required `[native_reference]` table: `hook`, `assigned` or `none`. The
  public API reference and the sessions page state the corrected rule for
  runtime packages without an integration handler (resume and fork only through
  an assigned reference), the assigned launch template and existence check, the
  `assigned`/`reported` provenance, and the `agent_native_reference_missing`
  error. The agent-profiles page notes that a profile inherits an assigned
  base's assignment. The rule that a later reported reference supersedes an
  assigned one is not part of this change.

## 0.33.1

### 2026-10-03, native launch spec

- Replaced the profile `[resume] mode`/`ref_kind` keys and the `[fork]` table
  with a typed native-session launch spec: `[resume] reference_kind`, `args`,
  and optional `fork_args`, each argv with one whole-token `{reference}`.
  `fork_args` is accepted only on bases with compiled fork support (`claude`)
  until runtime packages declare their own; `codex` and `hermes` profiles that
  set it are rejected. `resumable = false` now also removes fork, since fork requires resume. The
  agent-profiles page documents the built-in specs, the profile form, and the
  `invalid_profile` rejections; the public API `capabilities` note and the
  sessions fork paragraph follow the frozen spec.

## 0.33.0

### 2026-10-03

- Documented the macOS release shape: `aarch64-apple-darwin` CLI and daemon
  archives are ad-hoc signed (`signing adhoc` in the `MANIFEST`), not notarized,
  and every release asset, including the `.sha256` checksum files, has a
  GitHub build-provenance attestation
  (`gh attestation verify <file> --repo zajca/pohunek`). The install-on-macos
  runbook covers the Homebrew formula (`zajca/pohunek/pohunek`, with
  `pohunek service install|upgrade|uninstall` around brew operations), the
  `curl -fLO` archive path, and troubleshooting for Gatekeeper on browser
  downloads and the Keychain re-prompt after an upgrade. The source map drops
  the Developer ID keychain and notarization scripts and describes
  `packaging/macos/sign`, `verify-signed --adhoc`, and `package --adhoc-release`.

## 0.32.0

### 2026-09-29

- Moved the `pohunek attach` reconnect settings from `launcher.conf` to the
  core-owned `<config_dir>/attach.conf` (same keys, same validation, installed
  as a commented template by `pohunek setup config`). Core never reads
  `launcher.conf`; an operator who tuned `attach_reconnect_seconds`,
  `attach_reconnect_interval_seconds` or `attach_reconnect_max_attempts` there
  must copy the keys into `attach.conf`. The attach terminal behavior and the
  reconnect settings are documented in the sessions concept page.
- Removed the rofi/sway launchers from the bundle: the launcher guide and the
  launcher debug runbook are deleted, the launcher scripts leave the source map,
  and `pohunek setup` keeps only `config` and `completions` (a bare `setup` is
  `setup config`). `pohunek doctor` and `daemon.doctor` no longer report
  `bin:rofi`, `bin:swaymsg`, `bin:python3`, `bin:timeout`, `terminal`,
  `launcher_scripts` or `sway_include`; hook interpreter readiness stays in
  `integration.doctor`. The launchers are developed in `zajca/pohunek-work`.
- Documented the upgrade handoff for launcher assets that earlier releases
  installed: the update-after-release runbook lists the exact
  `<data_dir>/bin` scripts and the `<config_home>/sway/config.d/pohunek.conf`
  drop-in to remove (or hand over to the `pohunek-work` setup); core deletes
  nothing. Corrected the `stale-installation` and `config-not-installed` eval
  scenarios to describe only signals `pohunek health` and `prompt_not_found`
  actually produce.
- Removed the native GUI from the bundle: the GUI setup guide, the GUI review
  guide, the GUI attach-template section of the environment guide, the GUI
  provider-token section of the secrets policy, and the macOS GUI app bundle
  runbook parts. The native GUI is developed in `zajca/pohunek-work`; the public
  API reference lists it as an external client and names the core tests that
  cover `host.discover`, `subscribe`, and `worktree.remove`.
- Removed the web control center from the bundle: the web control center guide
  is replaced by the TypeScript SDK guide (package surfaces, WebSocket relay
  transport contract, loopback test relay, runtime paths), and the macOS
  install and update runbooks, the concept pages, the remote-hosts guide, and
  the source map no longer describe the web backend or its archive. The web
  control center is developed in `zajca/pohunek-work`.
- Stated the UI-less core policy in the architecture concept page: core ships no
  user interface, external clients in `zajca/pohunek-work` use public contracts
  only (CLI `--json`, and the public protocol through the Rust crates pinned by
  git tag and the TypeScript SDK release tarballs pinned by URL and integrity),
  move in lockstep with the protocol version, and link core crates that are a
  pinned, not a stable, API. Issue/PR providers live only in `zajca/pohunek-work`.
- Documented the owner backend's optional rotating owner-private log files
  (`POHUNEK_BACKEND_LOG_DIR` and its limits, failure fallback, torn-line
  recovery, repair at open, startup failures recorded in the files).
- Documented that the Bun owner backend resolves the daemon socket with the shared
  runtime-path contract (including the macOS default and the native socket path
  limit) and Node discovery for the dev stack.
- Documented that the `netbird` CLI is resolved by the trusted-executable policy
  (process `PATH`, then the macOS fallback directories) in the process that runs
  it, and that the doctor `netbird_cli` check uses the same lookup. Added the
  macOS host section of the remote-hosts guide (listener binding, sleep and wake,
  troubleshooting).
- Added the macOS install runbook: archives and what each installs, verification
  with `shasum`/`codesign`/`spctl`, daemon install, start/status/stop with the
  launchd label, GUI and web backend, upgrade without lost sessions, rollback,
  uninstall (refused with live sessions, `--stop-sessions` and `--purge`
  explicit), log locations, logout/reboot/sleep semantics, and Gatekeeper
  remediation that never disables it.
- Documented the owner web backend's macOS archive and launchd agent: separate
  client service labelled with the daemon namespace, `plutil`-written property
  list, allowlisted non-secret environment copied from `backend.env`, bounded
  private logs, `install.sh --uninstall` keeping configuration and logs, and the
  shared `packaging/verify-archive` preflight.
- Documented the macOS GUI app bundle (`Pohunek.app`, identifier
  `io.github.zajca.pohunek.gui`), how it finds the installed CLI, and the
  Developer ID signing and notarization pipeline of the release workflow
  (owner-protected `macos-signing` environment, ephemeral keychain, notary API
  key; missing credentials fail the macOS release jobs and nothing unsigned is
  published). macOS is not yet a published platform.
- Documented the native macOS package tooling: the deployment-target file, the
  Mach-O audit (arm64 only, deployment target, system libraries only, no
  build-machine paths), the unsigned development package, and the CI acceptance
  that installs, upgrades with live sessions, refuses bad archives, and
  uninstalls from the extracted archives against real launchd.
- Documented release archive integrity: every archive carries a `MANIFEST`
  (component, version, target, signing state, SHA-256 of each member) and
  `packaging/install-daemon.sh` verifies it, the host OS and architecture, the
  macOS minimum version, and member permissions before it runs any archive
  binary or changes anything; archives are deterministic (`packaging/archive`).
- Documented the reserved delegated task error contract: the `task_*`,
  `worktree_*` and `check_*` codes, their fixed text, `worktree_in_use` shared by
  `session.remove` and `worktree.remove`, and `task.*` still answering
  `method_not_found`.
- Documented the executable search-path policy: the `service.toml`
  `[environment] search_path` key, the `service_search_path_unavailable` error
  code, and the environment-resolution guide.
- Documented macOS platform diagnostics and setup: the platform-specific
  `pohunek doctor` and `daemon.doctor` check lists (Linux-only launcher probes
  omitted on macOS; `runtime_dir_private`, `socket_path_length`,
  `filesystem_access` with the Privacy & Security explanation,
  `worker_executable`, `launchd_domain`, `launchd_job`, `login_shell`,
  `terminal`, `desktop_notifications`, `keychain`), executable resolution that
  requires an execute bit, and `pohunek setup` skipping the sway/rofi steps on
  macOS with a successful, typed `skipped` outcome.
- Documented transcript indexing for external observation: a bounded
  reconciliation pass every 30 seconds (and on lost-event hints) converges the
  index independently of the inotify or FSEvents watcher, which only reduces
  latency; the per-pass scan bounds; and the `external_transcript_watcher_unavailable`
  and `external_transcript_watcher_degraded` causes.
- Documented `pohunek integration doctor` and `pohunek integration uninstall` for
  Codex and Claude (`integration.doctor` and `integration.uninstall`): stable
  findings with remediation, the macOS `python3` stub check that never runs the
  stub, the socket path limit check, and marker-owned, rollback-protected removal.
- Documented the Claude and Codex integration installer transaction: a
  per-config-directory lock file, rollback of committed files on failure, the
  distinct `integration_install_in_progress`, `integration_destination_collision`,
  and `integration_recovery_required` errors, an absent agent config directory
  reported as informational `not_installed` with recovery `none`, and the macOS
  remediation for symlinked config roots and Darwin socket path limits.
- Documented that Hermes hook identity reports use the platform's process start
  identity on Linux and macOS, matching the daemon's derivation.
- Documented that the managed agent hook scripts run Python in isolated mode, so
  modules in the agent's working directory and `PYTHON*` variables cannot affect
  the hook interpreter's configuration or module resolution.

### 2026-09-28

- Documented that a `session.remove` refused with `runtime_supervision_ambiguous`
  because same-user processes with unreadable environments may belong to the
  runtime now lists them (pid, start identity, command name; at most eight,
  then a count) in the error message and carries a `recover` hint.
- Documented `pohunek session rm --accept-unconfirmed-cleanup`, which calls the
  new `session.remove_accepting_unconfirmed` method (params `SessionId`, result
  `SessionRemoveResult`): per-call consent to remove a session although only
  unreadable-environment processes keep its marker sweep unconfirmed. Plain
  `session.remove` keeps refusing. The accepted processes (at most 64) are never
  signalled and are reported as `accepted_unconfirmed_processes`; reconciliation
  and the retention sweep never consent, and every other unconfirmed reason
  still refuses.

- Documented `pohunek service lock -- <command>`, which runs a command under
  the service transaction lock and lets its `pohunek service` calls adopt
  the lock with the holder token in `POHUNEK_SERVICE_LOCK_TOKEN`
  (`service_inherited_lock_invalid` for a token that proves no live holder),
  and `pohunek service check`, which runs the install or upgrade preflight,
  including the stale daemon-job check, without changing anything. Adopting
  commands hold `service-install.lock.adopted` shared, the holder waits for
  them before it releases the lock, adopted transactions are serialized by
  `service-install.lock.inherited`, and the command runs in its own process
  group (in the terminal's foreground when `service lock` owns it) so each
  signal reaches it exactly once; `Ctrl-Z` stops and `fg` resumes the lock
  and its command together, like a job.
- Documented that `packaging/install-daemon.sh` runs its legacy retirement and
  final install or upgrade under that lock and runs `service check` before it
  touches the legacy install.

### 2026-09-27

- Documented that a failed `session new` whose worktree compensation cannot
  finish stays listed as `reconnecting` with `create_compensation_pending`
  and is retried by the running daemon, and that `session new` and `session
  fork` fail with `migration_manifest_missing` while unimported legacy resume
  bindings exist.
- Documented that a session becomes `lost` only after its ended job is retired
  (otherwise `reconnecting`, `runtime_supervision_unavailable`, retried), and
  that reconciliation finishes an interrupted removal through the same
  retirement and cleanup as `session rm`, keeping the session listed and
  retried until they succeed.
- Documented that `session rm` of a `conflict`, `reconnecting`, or
  `incompatible` session retires the worker job of its recorded generation
  through the service manager before deleting the record, refuses a record that
  names no generation with its runtime code, and keeps the record with
  `runtime_supervision_unavailable` when retirement fails.
- Documented that `service uninstall` clears the transaction record before
  `service.toml`, uninstalls a registered pending install whose `service.toml`
  is gone, and refuses with `service_outdated_journals` while a journal of an
  earlier schema names a worker that may still run; such journals also keep
  every version directory during an upgrade.
- Documented the worker session ID grammar: `s-<ULID>` or `s-<digits>` with 1
  to 20 digits.

### 2026-09-25

- Documented that `migration preflight` takes an explicit `--socket <path>` so
  the archive installer can dial the legacy daemon after renaming its control
  socket aside as a connect barrier, retire it in a fail-closed order
  (barrier, preflight, worker inventory, stop, post-stop re-inventory), and
  abort without removing legacy files when a not-inactive template worker
  survives.

## 0.31.0

### 2026-08-12

- Documented that a Darwin process between images no longer fails a process
  table read: it is listed without a command line, its markers are
  unobservable, and the runtime sweep skips it as unreadable.
- Documented that cwd evidence is ordered by observation time: a procwatch read
  taken before an OSC 7 hint arrived no longer moves the session back.
- Documented native worker supervision (#100): every worker generation is its
  own systemd transient unit (`pohunek-<ns>-worker-<session-id>-<generation>.service`
  in `pohunek-<ns>-sessions.slice`, systemd 255 or newer) or launchd job
  (`io.github.zajca.pohunek.<ns>.worker.<session-id>.<generation>`, private
  definition, no `KeepAlive`); the `pohunek-session@.service` template and the
  `packaging/systemd` files are gone. Added `pohunek service
  install|upgrade|uninstall|status`, the versioned
  `<prefix>/libexec/pohunek/<version>/` layout, `service.toml`, the install
  journal, and `pohunek daemon start --dev-subprocess`; `pohunek daemon start`
  now requires an installed service.
- Documented the lifetime scope: terminal close and screen lock are safe;
  logout and reboot end workers, which the next login reports as `lost` with
  `runtime_lost` and never restarts. Added the reconciliation reasons
  `runtime_supervision_unavailable`, `runtime_supervision_ambiguous`,
  `runtime_identity_mismatch`, `runtime_lost_cleanup_unconfirmed`,
  `worker_job_absent`, and `stale_worker_generation`, and the
  manual macOS acceptance procedure in `docs/acceptance/`.
- Documented that agents no longer inherit the daemon's whole environment:
  the worker builds the child environment from an empty base plus the
  allowlisted `BaseEnv` (`[environment] allowlist` in `service.toml`), `TERM`,
  the profile environment, and its `POHUNEK_*` identity, and always strips
  service-manager variables.
- Documented portable PTY readiness in the session worker: one `poll(2)`
  implementation over the PTY master and a cancellation self-pipe on Linux and
  Darwin, with the worker's complete test suite running on native macOS CI.
  Darwin kernel peer identity is recorded as in place; launchd, clients,
  packaging, and native acceptance remain deferred through #100-#105.
- Documented that both process backends keep reporting an exited but unreaped
  process with its identity, marked as no longer running.
- Documented automatic session retention: the conservative opt-in default
  policy, the separate terminal and lost TTLs, `session policy get`/`set`,
  `session retention sweep --dry-run`/`--apply`, and the eligibility rules that
  keep external, conflicting, incompatible and in-TTL sessions untouched.
- Documented the sweep's unsaved-work hold and its truthful counters: a matched
  session whose owned worktree has uncommitted, untracked or unpushed work — or
  a state git cannot report — is held rather than removed and carries a `hold`
  reason, `worktrees_cleaned` counts only checkouts confirmed gone with a
  surviving checkout reported as `worktrees_failed`, and
  `session retention sweep` exits non-zero on a partial failure.
- Documented the native Darwin process backend and the single shared host
  inspector: `libproc` process records, `sysctl` `KERN_PROCARGS2` argument and
  environment regions parsed separately, bounded allocations and traversal,
  checked kernel start-identity encoding interpreted identically by daemon and
  worker, controlling-terminal foreground groups resolved without assuming the
  group equals the root process, and an event-driven kqueue `NOTE_EXIT` exit
  watch that re-verifies process identity after registration. Complete macOS
  host, WebUI, packaging, and release support remains deferred through #98-#105.
- Documented the Darwin process-inspection privilege boundary: ownership decided
  by the unprivileged short process record, another user's process reported as
  absent while a refused fact about an owned process stays a denial, and the
  three facts macOS withholds without a privilege pohunek does not request
  (a foreign process's working directory and argument vector, and a
  code-signing-restricted process's environment). An argument vector an `exec`
  or an exit withholds leaves that one process without a command line instead of
  failing the inventory it appears in.
- Documented the implemented provider-neutral relay account linking: identity is
  exactly issuer plus immutable subject, provider profile attributes never link
  accounts, both the current relay actor and the new identity prove themselves in
  one audited transaction completable only through its own browser or device
  channel, and a link or unlink takes effect on current credentials and sessions
  immediately. Recorded the HTTPS start/poll/status/cancel/unlink surface with no
  CLI subcommand, the durable PostgreSQL guarantees, and the user-facing safety
  rules. Kept host links, routing, attach, team administration, provider
  verification, and team clients explicitly deferred.
- Documented the shared secure runtime-path and portable filesystem foundation:
  preserved XDG durable locations, the owner-private macOS runtime default,
  fail-closed path validation, byte-bounded Unix sockets, atomic no-replace
  installation, and explicit uncertain-durability handling. Complete macOS
  host, WebUI, packaging, and release support remains deferred through #97-#105.
- Added the bundled agent-skill guide: safe agent CLI operation covering
  discovery, explicit targeting, JSON state reads, event subscriptions,
  send-and-wait flows, worktree diff inspection, destructive-op caution, and
  owner-first approvals. The guide is the hand-authored source for the
  checked-in CLI skill artifact guarded by xtask drift checks. The
  `pohunek agent-skill` command prints that artifact verbatim, or with
  `--json` as one envelope carrying the skill text and its `content_sha256`;
  the command is fully local and the global `--host` flag is accepted and
  ignored.
- Documented the accepted complete macOS host/client contract and GitHub-owned
  #94-#105 delivery tracking through the issue hierarchy, milestone, and
  Project. Recorded the shipped shared-platform/Linux migration and native
  Darwin contract CI while keeping full macOS support explicitly unshipped
  until native release acceptance closes.
- Documented the implemented reduced relay foundation: PostgreSQL fencing and
  recovery, protected initial Owner/service-account provisioning, generic OIDC
  browser and device authentication, bounded HTTPS account and credential
  lifecycle, and the native HTTPS/keyring credential CLI. Kept host links,
  routing, attach, account linking, complete team administration, provider
  verification, and team clients explicitly deferred.
- Documented shipped host-local stable identity and safe governance inspection:
  explicit never-enrolled state, exact principal-or-team owner, checked
  revisions, quarantine, owner-private persistence, doctor checks, CLI, SDK,
  GUI, and transparent owner-WebUI boundaries. Kept relay, team, enrollment,
  transfer, and share APIs explicitly unshipped.
- Added the accepted optional team-relay concept and separated its future
  protocol-v4, multi-team, `HostShare`, and trusted-relay boundaries from the
  currently shipped owner-only protocol v3 and transparent Bun web backend.
- Documented the simplified session-first native GUI: prioritized cross-host
  groups, modal session detail, direct lifecycle actions, and removal of the
  Agents monitor, provider browsers, review, and worktree-management surfaces.
- Documented standard Tab/Shift+Tab form focus, Enter/Ctrl+Enter submission,
  modal focus containment, and mouse selection/copy for native GUI detail text.
- Documented source-locked, credential-free Hermes compatibility CI and
  extracted release-archive plugin smoke verification.
- Documented configurable Hermes plugin timeout, output, screen, and concurrency
  bounds plus protocol-range repair during integration update.
- Added the Hermes operator guide: explicit managed runtime selection, isolated
  plugin lifecycle, access and host policy, complete typed tool surface,
  origin-session protection, bounded control-loop recovery, and payload-free
  lifecycle reporting.
- Clarified that native GUI releases target glibc x86_64 because Wayland and
  graphics libraries remain dynamic runtime dependencies; MUSL archives remain
  available for the CLI and daemon.
- Documented durable per-session PTY workers, daemon restart reconciliation,
  runtime state and events, explicit native recovery, systemd diagnostics, and
  the one-time legacy migration boundary.
- Documented browser session lifecycle and metadata management, the host-scoped
  Projects screen, and daemon-authoritative worktree removal safeguards.
- Documented the M1 web control center, browser-safe TypeScript SDK entry,
  backend deployment boundary, and fixture-backed development stack.
- Documented the self-contained Linux x86_64 web release archive and its
  systemd user-service installation flow.
- Documented that the native GUI `Start session` and Review "Dispatch as
  session…" agent pickers use the selected host's launchable runtime inventory,
  including resolvable profiles. The GUI fails closed when that inventory is
  unavailable instead of deriving launch permission from the name-only
  `supported_agents` field or falling back to built-in base kinds.
- Cross-linked agent profiles from the GUI Session Launch section.
- Documented fail-closed daemon shutdown when an overlay listener supervisor
  exits or panics unexpectedly after readiness.

## 0.18.3

- Documented native GUI keyboard list navigation and configurable
  `[keybindings]` overrides in `gui.toml`, including default binding names,
  key-string syntax, and same-context conflict validation.
- Linked GUI keyboard/config/reducer/provider source files from the assistant
  source map.

## 0.15.3

- Documented session notification dedupe/debounce behavior for
  `turn:<session_id>`: turn-completed creates share the debounce window,
  resolve on session resume, supersede older unread turn rows, and are collapsed
  by visible attention records.

## 0.15.2

- Documented the attention notification debounce window
  (`attention_debounce_secs`), the deferred-create behavior for
  `agent_blocked`/`approval_required` (`created: true` with a minted id, held
  pending until it flushes or is suppressed by an in-window resolve), and its
  relationship to `attention_dedupe_window_secs` in the sessions concept, the
  debug-daemon runbook, and the public API reference.

## 0.14.5

- Documented the durable cross-host notification Inbox, CLI/API surface,
  provider hook requirements, source-priority dedupe, retention, and trust-model
  boundary.

## 0.7.4

- Documented that the native GUI `Start session` runtime picker supports the
  built-in `shell`, `codex`, and `claude` choices.

## 0.7.3

- Documented that `pohunek-gui` is Wayland-only and fails fast when
  `WAYLAND_DISPLAY` is missing or empty.

## 0.7.2

- Documented explicit `session.resume` and the GUI "Open in terminal" behavior
  for terminal sessions with native resume metadata.

## 0.7.1

- Documented that release archives include the native `pohunek-gui` binary and
  linked release packaging files from the assistant source map.

## 0.7.0

- Added native GUI provider-integration guidance for Linear and GitHub, including
  `gui.toml` provider configuration, keyring and `gh` boundaries, request
  staleness handling, provider launch flow, `link.*` metadata, and PR status
  degradation rules.
- Linked the new GUI provider implementation and provider test files from the
  assistant source map.

## 0.5.0

- Added GUI setup guidance for `pohunek-gui`, including `gui.toml`, external
  attach delegation, Wayland startup diagnostics, project/worktree method
  boundaries, and secret-handling rules.
- Added native GUI prompt-management guidance covering host-resolved
  prompt/action browse, preview rendering through `crates/prompt`, and
  `session.new input` launch behavior.
- Linked GUI setup from the knowledge index and source map.

## 0.3.3

- Added the Phase 1 hand-authored knowledge skeleton for the Universal Pohunek
  Assistant.
- Reserved `index.md` for navigation and `log.md` for bundle history.
- Kept generated reference content out of the committed tree.
