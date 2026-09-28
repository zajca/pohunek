# Manual acceptance evidence

Some guarantees cannot be automated on hosted CI because they need a real
login session to end. This directory holds the structured evidence of those
manual runs. Each evidence file is produced by a script under
`scripts/acceptance/` and is committed only after an operator has run the full
procedure on real hardware. Never write or edit an evidence file by hand.

| Evidence | Procedure | Issue |
|----------|-----------|-------|
| `100-launchd-lifetime.json` | `scripts/acceptance/macos-launchd-lifetime` | [#100](https://github.com/zajca/pohunek/issues/100) |

## macOS launchd worker lifetime (#100)

The procedure proves the documented lifetime scope of launchd-supervised
session workers (see `docs/design/macos-support-rfc.md`, "Worker lifetime"):

| Phase | Operator action | Expected outcome |
|-------|-----------------|------------------|
| `screen-lock` | Lock the screen for at least 60 s, then unlock. | Every session survives with the same worker generation, worker PID, runtime ID, and child PID. The daemon keeps its PID. Each shell PTY still runs a typed command. |
| `terminal-close` | Attach to one session from a new terminal window, then quit the terminal application. | The same survival outcome as `screen-lock`. |
| `logout-login` | Log out and log back in. | The daemon login agent starts again (new PID) and reconciliation marks every session `lost` with reason `runtime_lost`. No worker job or worker process exists for any session, and `launchctl print` reports each old label absent (status 113). Explicit `pohunek session resume` of a session with a native reference starts a new worker generation. |
| `reboot` | Restart the Mac and log in. | The same outcome as `logout-login` for a fresh session set. The boot time must change. |

### Prerequisites

- A Mac on a supported macOS version, logged in at the GUI (the `gui/<uid>`
  launchd domain must exist). Run the procedure from a Terminal window, not
  over SSH.
- The service installed with `pohunek service install`, with `pohunek service
  status` reporting the daemon job `running`.
- The Xcode Command Line Tools (`xcode-select --install`), which provide
  `/usr/bin/python3` for the evidence helper.
- At least one agent profile with native resume (`claude` or `codex`) whose
  session-capture hook is installed (`pohunek integration install`). Its
  captured native session reference is what makes explicit recovery possible;
  shell sessions have no native resume.
- "Reopen windows when logging back in" unticked for the logout and reboot
  phases, so no terminal restores an old attach by itself.

### Running

```sh
scripts/acceptance/macos-launchd-lifetime start --agent claude
```

`start` records the host, prepares session set `a` (two shell sessions plus
one session per `--agent`, all started in `$HOME`), waits until every session
is live and every resumable session has reported its native reference, and
begins the first phase. Each phase prints its operator instructions. Quitting
the terminal, logging out, and rebooting end the script; afterwards open a
terminal and run:

```sh
scripts/acceptance/macos-launchd-lifetime continue
```

Before the after-snapshot, `continue` asks you to confirm that the action was
done and accepts an optional one-line note. It then waits for the daemon to
answer and for reconciliation to settle. The reboot phase prepares a fresh set
`b` because the logout phase ends set `a`. After each lost phase, the script
resumes every recoverable session to prove explicit recovery, records the
result, and stops the recovered session again.

Other commands:

| Command | Effect |
|---------|--------|
| `status` | Print the recorded phase and assemble the evidence from what is recorded so far. |
| `cleanup` | Remove every session the run created (`pohunek session rm`). |

Options: `--state-dir DIR` (default
`${XDG_STATE_HOME:-$HOME/.local/state}/pohunek-acceptance/100-launchd-lifetime`,
which survives a reboot), `--output FILE` (default
`docs/acceptance/100-launchd-lifetime.json` in this checkout), `--pohunek BIN`,
`--python BIN`, and `--shell-sessions N` for `start`. A second `start` refuses
to reuse an existing state directory; move it away to start over.

The script writes the evidence file after every completed phase. Commit it only
when `complete` and `passed` are both `true`. A failed run is still useful: keep
the state directory and report the failing checks, which the script prints
after every phase.

### How observations are taken

Everything the evaluation uses is recorded raw in the state directory under
`phases/<phase>/` and is interpreted only by
`scripts/acceptance/launchd_lifetime_evidence.py`:

- `before/` and `after/`: `pohunek session list --json`,
  `pohunek service status --json` (the daemon job and one worker job per
  generation), and `sysctl -n kern.boottime`;
- `after/ps.txt`: `ps -axww -o pid=,ppid=,command=`, searched for any
  `pohunek-sessiond` of the phase's sessions;
- `after/launchctl.txt`: the `/bin/launchctl print gui/<uid>/<label>` exit
  status of each session's old worker label (only the status is recorded, the
  output is discarded);
- `probe/`: the token typed into each shell session with
  `pohunek session input`, and the resulting `session screen --json` and
  `session output --json`;
- `recovery/`: the `pohunek session resume --json` result and exit status of
  each recoverable session, and the snapshots taken after recovery.

The evaluation logic is unit-tested in
`scripts/tests/test_launchd_lifetime_evidence.py`.

### Evidence schema (`schema_version` 1)

Top level:

| Field | Type | Meaning |
|-------|------|---------|
| `schema` | string | Always `pohunek.acceptance.launchd-lifetime`. |
| `schema_version` | integer | `1`. Incremented on any incompatible change. |
| `issue` | integer | `100`. |
| `run_id` | string | `<UTC start>-<short host name>`. |
| `started_at` / `completed_at` | string / null | RFC 3339 UTC. `completed_at` is set only when every phase is recorded. |
| `complete` | boolean | All four phases were recorded, in order. |
| `passed` | boolean | `complete` and every phase passed. |
| `host` | object | `product_name`, `product_version`, `build_version` (`sw_vers`), `arch` (`uname -m`), `hardware_model` (`sysctl -n hw.model`), `cpu` (`machdep.cpu.brand_string`), `uid`. |
| `pohunek` | object | `cli_version` (`pohunek --version`) and the `active_version`, `namespace`, and `prefix` from `pohunek service status --json`. |
| `phases` | array | One object per recorded phase, in execution order. |

Phase object:

| Field | Type | Meaning |
|-------|------|---------|
| `name` | string | `screen-lock`, `terminal-close`, `logout-login`, or `reboot`. |
| `expectation` | string | `survive` or `lost`. |
| `started_at`, `action_confirmed_at`, `completed_at` | string | RFC 3339 UTC. |
| `operator_note` | string | The optional note entered at confirmation. |
| `checks` | array | Phase-level checks (below). |
| `sessions` | array | One object per session of the phase. |
| `passed` | boolean | Every phase check and every session passed. |

Phase-level checks: `sessions_prepared`; `host_not_rebooted` (all phases except
`reboot`) or `host_rebooted` (`reboot`, boot time changed); `daemon_unchanged`
(survival phases: the daemon job is `running` with the same PID),
`daemon_restarted_at_login` (`logout-login`: `running` with a new PID), or
`daemon_running_after_login` (`reboot`); `recovery_covered` (lost phases: at
least one session had a native recovery reference); and
`observations_readable` / `recovery_observations_readable`, which appear only
when a recorded file is missing or malformed.

Session object:

| Field | Type | Meaning |
|-------|------|---------|
| `session_id`, `agent`, `agent_base` | string | From the before snapshot. |
| `before`, `after` | object | `present`, `session_state`, `runtime_state`, `runtime_id`, `loss_reason`, `root_pid` (the agent child), `worker_generation`, `worker_state`, `worker_pid`. |
| `liveness_probe` | object / null | Survival phases, shell sessions only: `attempted`, `passed`. |
| `old_generation` | object | Lost phases: `label`, `launchctl_print_status`, and `worker_processes` (matching `ps` lines, expected empty). |
| `recovery_expected` | boolean | Lost phases: the session had native resume and a captured native reference before the action. |
| `recovery_available` | boolean | Lost phases: `session.resume` would accept the session after the action. |
| `recovery` | object / null | Lost phases, when expected: `attempted`, `exit_status`, and the resulting `runtime_state`, `runtime_id`, `worker_generation`. |
| `checks` | array | Session checks (below). |
| `passed` | boolean | Every session check passed. |

Every check is `{"name": string, "passed": boolean, "detail": string}`.
Session checks are `present_before`, `present_after`, and
`one_worker_job_before`. Survival phases add `runtime_live_after`,
`same_runtime_id`, `same_worker_generation`, `same_worker_pid`,
`same_child_pid`, and, for shells, `pty_accepts_input_and_prints`. Lost phases
add `runtime_lost`, `loss_reason` (exactly `runtime_lost`),
`no_worker_job_after` (no job of any generation, so no resurrection and no
automatic restart), `old_generation_label_absent`, `no_worker_process`, and,
when recovery is expected, `recovery_available` and
`explicit_recovery_starts_new_generation` (resume exited 0, the runtime is
`live` with a new runtime ID, and the worker generation differs from the lost
one).
