# Manual acceptance evidence

Some guarantees cannot be automated on hosted CI because they need a real
login session to end. This directory holds the structured evidence of those
manual runs. Each evidence file is produced by a script under
`scripts/acceptance/` and is committed only after an operator has run the full
procedure on real hardware. Never write or edit an evidence file by hand.

| Evidence | Procedure | Issue |
|----------|-----------|-------|
| `100-launchd-lifetime.json` | `scripts/acceptance/macos-launchd-lifetime` | [#100](https://github.com/zajca/pohunek/issues/100) |
| `1112-tailscale-overlay.json` (added by the acceptance run, not by the change that adds the procedure) | `scripts/acceptance/tailscale-overlay` | [#1112](https://github.com/zajca/pohunek/issues/1112) |

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
- At least one agent profile with native resume whose session-capture hook is
  installed. `claude`, `codex` and `hermes` are served only by their official
  packages from the release archive. Its captured native session reference is what makes explicit
  recovery possible; shell sessions have no native resume.
- "Reopen windows when logging back in" unticked for the logout and reboot
  phases, so no terminal restores an old attach by itself.

### Running

```sh
scripts/acceptance/macos-launchd-lifetime start --agent <runtime>
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
| `before`, `after` | object | `present`, `session_state`, `runtime_state`, `worker_instance_id`, `loss_reason`, `root_pid` (the agent child), `worker_generation`, `worker_state`, `worker_pid`. |
| `liveness_probe` | object / null | Survival phases, shell sessions only: `attempted`, `passed`. |
| `old_generation` | object | Lost phases: `label`, `launchctl_print_status`, and `worker_processes` (matching `ps` lines, expected empty). |
| `recovery_expected` | boolean | Lost phases: the session had native resume and a captured native reference before the action. |
| `recovery_available` | boolean | Lost phases: `session.resume` would accept the session after the action. |
| `recovery` | object / null | Lost phases, when expected: `attempted`, `exit_status`, and the resulting `runtime_state`, `worker_instance_id`, `worker_generation`. |
| `checks` | array | Session checks (below). |
| `passed` | boolean | Every session check passed. |

Every check is `{"name": string, "passed": boolean, "detail": string}`.
Session checks are `present_before`, `present_after`, and
`one_worker_job_before`. Survival phases add `runtime_live_after`,
`same_worker_instance_id`, `same_worker_generation`, `same_worker_pid`,
`same_child_pid`, and, for shells, `pty_accepts_input_and_prints`. Lost phases
add `runtime_lost`, `loss_reason` (exactly `runtime_lost`),
`no_worker_job_after` (no job of any generation, so no resurrection and no
automatic restart), `old_generation_label_absent`, `no_worker_process`, and,
when recovery is expected, `recovery_available` and
`explicit_recovery_starts_new_generation` (resume exited 0, the runtime is
`live` with a new runtime ID, and the worker generation differs from the lost
one).

## macOS package install, upgrade, and uninstall (#104)

`scripts/acceptance/macos-package-install` runs in CI (job `macOS package
install and upgrade (arm64)`) against the real `gui/<uid>` launchd domain and
needs no operator. It extracts two daemon archives (version A and a newer
version B, both built by `packaging/macos/package --development`) into an
isolated root and checks, from the extracted archives alone:

| Step | Outcome checked |
|------|-----------------|
| Two concurrent installers, prefix containing a space and a quote | One healthy installation: one launchd job, the agent property list valid and starting the version A daemon by its exact path; the loser is refused because another `pohunek service` command is running, or runs afterwards as an unchanged upgrade. |
| Installer run again | The daemon keeps its process. |
| Corrupt, member-less, other-architecture, and group-writable archives with two live sessions | Refused before any change: daemon, workers, and session children keep PID, start time, and PTY. |
| Upgrade A to B with two live sessions | The daemon is replaced; both workers and both session children keep PID, start time, PTY, and version A; version A stays installed; a new session runs from version B. |
| Uninstall with live sessions | Refused, naming the sessions; nothing changed. |
| Uninstall after the sessions stopped | Daemon job, property list, versions, `<prefix>/bin/pohunek`, and `service.toml` removed; the durable state directory stays. |

Interrupted installs and upgrades at every journaled step are covered by the
service engine tests, not by this script. Run it locally on a Mac with
`PKG_ACCEPT_ARCHIVE` and `PKG_ACCEPT_UPGRADE_ARCHIVE` set; it exits 2 when no
`gui/<uid>` domain exists.

## Tailscale as an opt-in overlay (#1112)

`scripts/acceptance/tailscale-overlay` is the D9 acceptance run of the Tailscale
overlay provider. It needs no root on any machine and runs real binaries over
real tunnels. The pohunek binaries come from the `pohunek-bins` artifact of a CI
run of the commit under test (`--ci-run`), never from a host build, because the
guests carry Debian 13's glibc 2.41. Every command it runs is recorded as a step
and interpreted only by `scripts/acceptance/overlay_evidence.py`; nothing is
judged by the shell script and no evidence is written by hand.

| Leg | What it proves |
|-----|----------------|
| `vms` | Two qemu VMs on a closed lab link, one running Headscale, both running a kernel-TUN `tailscaled` and a service-installed daemon with `providers = ["tailscale"]` and `port = 18733`. D4 in both directions: `doctor`, the single listener on the node's own address, `host list`, `host discover`, `host inspect`, `--host` with the short name, the FQDN, `tailscale:<name>` and `tailscale:peer~<unpadded base64url of the node id>`, `session new/input/screen`, `attach`, the SDK control path (`crates/client/examples/overlay_probe.rs`: registry, `resolve_host_target_in`, `Client::connect_with_registry`, handshake, `session.list`, `daemon_doctor`, `session_screen`), and `session stop/rm`. Recorded, never gating: the D7 unreachable hint (peer daemon stopped) and the D5 rebind (`tailscale down` then `up`, no daemon restart): after `down` the harness waits up to 90 s for the daemon's "overlay state unavailable; serving local-only" journal line, only then runs `up`, waits up to 180 s (the daemon rebinds on a retry backoff of 5 s doubling) for a fresh "serving control protocol over overlay" line and for a remote `health` from the peer. The rebind and the peer health gate the run; the drop line is recorded without gating. The daemon revalidates its listener every 30 s, so a short outage between two checks is, correctly, not logged. |
| `netbird` | The absent `overlays.toml` default (NetBird, port 18722) with a throwaway daemon in a clean environment under `/dev/shm/ph1112`, a path guard before anything starts, and the owner's daemon PID, NetBird peer counts and `health` checked unchanged before and after. |
| `macos` | zajcaair without root: the D6 trusted-CLI lookup with no `tailscaled` (found through `/opt/homebrew/bin`, state unavailable, `host list` fails, no listener on 18733) and the userspace-networking fail-closed path (`BackendState=Running`, `TUN=false`: doctor warns, `host list` fails with `tailscale_state_unavailable`, no listener). Needs `vms` because Headscale runs in VM-A. |

The userspace sub-step reaches the Tailscale CLI through a test shim: the CLI
has no environment override for its socket, and only root can create the
default darwin socket, so an owner-private wrapper in the throwaway `bin`
directory runs `tailscale --socket=<throwaway>`. The evidence labels it as
such. A full macOS tailnet node with a root `tailscaled` is not part of this
run.

### Prerequisites

- Linux x86_64 with KVM (`/dev/kvm` readable and writable), `qemu-system-x86_64`,
  `qemu-img`, `genisoimage`, `ssh`, `ssh-keygen`, `socat`, `objdump`, `curl`,
  `gh` (logged in), `python3`, `file`, `tar`; `jq` and the `netbird` CLI for the
  `netbird` leg. Nothing is installed by the script.
- A static `overlay_probe` for the guests, built under the heavy-run slots:
  `cargo build --release -p pohunek-client --example overlay_probe --target x86_64-unknown-linux-musl`.
  The script refuses a probe that `file` does not report as statically linked.
- For `netbird` and `macos`: the host's own NetBird address (`--netbird-address`,
  an address in `100.64.0.0/10` that this host holds), the owner's daemon unit
  (`--owner-unit`, only read), and the owner's `pohunek` CLI. For `macos`
  additionally a directory on the Mac with `pohunek`, `pohunekd` and
  `pohunek-sessiond` built for the commit under test (`--macos-bin-dir`), and the
  NetBird policy admitting the Mac to one extra TCP port on this host's NetBird
  address for the window below. If the policy or a firewall blocks it, the
  userspace sub-step is inconclusive and changing the policy is the owner's
  decision; the D6 sub-step does not need the port.
- A heavy-run slot wrapper (`--slot-wrapper /tmp/heavy-run.sh`) or an explicit
  `--no-slot-wrapper`. There is no default. Each `qemu` runs in the foreground
  under the wrapper, so the slot lasts as long as the VM. Each runner writes
  `slot-<vm>.held` once it holds its slot and waits for `go`, which is written
  only when both are held, so one VM never idles while the other waits. After 30
  minutes without both slots the run stops as inconclusive with nothing booted.

### Running

```sh
scripts/acceptance/tailscale-overlay run \
  --leg vms --leg netbird --leg macos \
  --slot-wrapper /tmp/heavy-run.sh \
  --ci-run <run id of the merge commit> \
  --probe <path>/overlay_probe \
  --netbird-address <this host's NetBird address> \
  --owner-unit pohunek-<id>-daemon.service \
  --macos-bin-dir <dir on the Mac> \
  --macos-project <project on the Mac, not the owner's working checkout>
```

`pins` prints the pinned versions and checksums. The command prints the verdict
and each failed, missing or inconclusive check, writes the evidence document to
`docs/acceptance/1112-tailscale-overlay.json` (`--output`), and keeps the raw
observations in `<run root>/ph1112-state-<UTC time>` (no key material; it passes
the same secret scan as the evidence). The VM disks, seeds and keys live in
`<run root>/ph1112-run-<UTC time>` and are deleted at teardown.

### Pins

Each pin sits at the top of the script and is re-verified on every run
(downloads are checked before use; a mismatch makes the run inconclusive). They
were re-verified against their upstream checksum sources on 2026-10-24.

| Component | Pin | Checksum source |
|-----------|-----|-----------------|
| Headscale | `v0.29.5`, asset `headscale_0.29.5_linux_amd64` | sha256 `e979d27fe17d924d33bb6ab7504f6338d0635989a0cfdcfbc0ceb637dbcbe980`, the release's `checksums.txt` |
| Tailscale (VMs) | `1.104.2`, `https://pkgs.tailscale.com/stable/tailscale_1.104.2_amd64.tgz` | sha256 `512f613ed7d04fc7282f7dc5bd76aa105aae28168fbb9357eb749ebcbc29f0ae`, the vendor `.sha256` file next to the tarball |
| Tailscale (zajcaair) | Homebrew `tailscale` | the Homebrew bottle checksum; the version is read with `brew list --versions` before anything uses it, and any other version makes the macOS leg inconclusive (`macos_tailscale_pin_mismatch`) |
| VM image | Debian 13 genericcloud amd64 `20261021-2638` | SHA512 `16e0965312c56feb331049d45d2b1b5778f3f942a33d4300eedbb97527707f2bea87c0006f3e4cc7a25fadbff739c4e235a51fb9bcd1b66fd554f7f8e80ba45c`, `SHA512SUMS` of the dated directory (identical in `latest`) |
| Offline packages (VMs) | the 11 `.deb` files below, from `https://snapshot.debian.org/archive/debian/20261021T000000Z/<pool path>` (fallback: the same pool path on `deb.debian.org`) | sha256 of each file, from the trixie `Packages` index of that snapshot; each is verified before use whichever source served it |

The genericcloud image lacks two things the run needs, and the guests have no
network egress, so the run installs the packages below offline with one
`dpkg -i` (it exits non-zero when a dependency is unmet, and the run stops
there):

- `dbus-user-session`: without it the systemd user manager has no session bus
  socket and `pohunek service install` fails with `supervisor operation connect
  is unavailable` (Debian's `libpam-systemd` only recommends the package, so it
  is missing on installs made without recommended packages). After the install
  the run restarts `user@<uid>.service` and waits (bounded) until `dbus.socket`
  is active and `/run/user/<uid>/bus` exists.
- `git` and the part of its dependency closure the image lacks: a remote
  `session new` needs a project (`--project`), and `project add` needs a git
  repository. Each VM gets a throwaway repository with one commit under the ph
  user's home, registered with `pohunek project add`, and D4 starts its session
  on the peer's project.

The set was computed against the image's package manifest
(`debian-13-genericcloud-amd64-20261021-2638.json`, SHA512
`614b8865e7496c6e6380b491a8f4121bd7ac654dd570a889f0684c70a946c847c0591edf223f3535afe3572e31370459310b8c7ffaba859ee5f40465c4062911`,
whose 322 packages equal the image's dpkg status) and the snapshot's `Packages`
index; the rule is recorded next to the pins in the driver. A failure to stage,
install or register is `provisioning_failed`.

| File | sha256 |
|------|--------|
| `dbus-user-session_1.16.2-2_amd64.deb` | `0f9aa775a708b28b1a3ad784e9d1f0c96227521bc29e99ddbfe05ff008b5d246` |
| `git_2.47.3-0+deb13u1_amd64.deb` | `3e35662fd5c46add561703e54031a1d8ad9df45811927689f0a51122b13be722` |
| `git-man_2.47.3-0+deb13u1_all.deb` | `b58306b60e1dff920e1e18bc7e1e499f2ef75184190ab3d98657b165969bf7c9` |
| `libcurl3t64-gnutls_8.14.1-2+deb13u5_amd64.deb` | `19da93544615a4c9a3d7df473f5bf8ebca99442454622e9b0460168dadf7b8ae` |
| `liberror-perl_0.17030-1_all.deb` | `06110a972bdc52f6c6d6615dddbe97f836c61d0e36c685ef4107c2eaa81f871f` |
| `libgdbm-compat4t64_1.24-2_amd64.deb` | `2cbd43cf2dfbf57ff48188b5d79d29cf7ea8f0dedaa61ab0bcc02eceff50ea01` |
| `libngtcp2-16_1.11.0-1+deb13u1_amd64.deb` | `627eec81ebbd48c4e6091f5cd9dc5070b792b7075000eed60ab08c7daa961caf` |
| `libngtcp2-crypto-gnutls8_1.11.0-1+deb13u1_amd64.deb` | `2a7f109c0c4db6a800e4661c5e5e34e1f1f83c8162482276183d1ada9da7c96c` |
| `libperl5.40_5.40.1-6+deb13u1_amd64.deb` | `e44777ff47ad248d1debd57e1f96ab5e89061a341008ae39880ba3b5fc15f273` |
| `perl_5.40.1-6+deb13u1_amd64.deb` | `ef145c247787d6f40dcdefd5b417502484827cde0d555998c17fd5b27ec1e1dd` |
| `perl-modules-5.40_5.40.1-6+deb13u1_all.deb` | `4c46cdd8f3135aa1c95f07e817f8e5d9be590687441ac1c27e3a8c49400f35ce` |

The download cache is `/mnt/m2_4t/t/ph1112-cache` (`--cache-dir`) and survives
runs.

### Resource budget

At most 2 vCPU and 1.5 GiB per VM (4 vCPU and 3 GiB together), both heavy-run
slots for roughly 25 minutes, about 1 GiB of overlay writes per VM, and a cache of
about 400 MB for the image plus about 110 MB of tarballs. Rough wall time:
download 5 min the first time, boot 2 min, provisioning 4 min, D4 and the extras
10 min, the NetBird check 3 min, the zajcaair leg 15 min.

### Network and exposure

- The VMs have no internet: `restrict=on` removes all guest egress while the
  forwarded SSH port keeps working. Every binary is copied in over SSH. SSH host
  keys are generated per run and pinned in a per-run `known_hosts`.
- VM-to-VM traffic uses one UDP datagram link through loopback sockets (no
  TCP-over-TCP under WireGuard). The embedded DERP region exists only because
  Headscale refuses an empty map; it cannot work over plain HTTP, so the two
  VMs need a direct path. If `tailscale ping` does not reach one, the run is
  inconclusive, not a product failure.
- Headscale serves its control protocol over plain HTTP. The control server's
  Noise key is therefore fetched without server authentication (trust on first
  use). That is acceptable only on the closed lab link and on the owner's NetBird
  network, and it is a lab-only property: a plain-HTTP control plane is the only
  one a non-root macOS user can use, because trusting a private CA there needs an
  administrator.
- Headscale's gRPC listener is bound to loopback inside VM-A and no API key
  exists. Every host-side port is bound to `127.0.0.1`.
- The one exception is the macOS window. For the userspace sub-step the script
  adds a runtime `hostfwd` through the qemu monitor that listens only on this
  host's NetBird address and forwards to Headscale's HTTP port. It exposes
  nothing but that control endpoint, joining needs an explicit approval on VM-A
  (`headscale auth register`), and no pre-auth key ever leaves VM-A. The script
  verifies with `ss` that the forward is bound to exactly that address, closes it
  right after the sub-step (or on any failure), and it dies with VM-A.
- Pre-auth keys are single-use, expire after 15 minutes, and travel through a
  pipe from VM-A to the joining VM. They are never written on the host or
  printed; the script never uses `set -x`.
- The script signals only processes of its own run, by exact PID taken from the
  qemu pidfile or `/proc` and re-checked against the run directory in the
  command line before each signal, and never uses `pkill`. It does not touch the
  owner's daemon unit or NetBird: the throwaway daemon of the `netbird` leg runs
  with `env -i` under `/dev/shm/ph1112`, and the leg refuses to start when that
  directory already exists.
- On zajcaair all commands run in a named shell session
  (`ph1112-<run time>`, `--cwd /tmp/ph-macos-cache`). The session is the only
  channel to the Mac, so it is started through the owner's own daemon there; a
  remote `session new` needs a project, which the operator names with
  `--macos-project` (no default; the session runs in the cache directory, not in
  that project's checkout, and nothing else is done through the owner's daemon
  except a final `health`; the one known side effect on the owner's daemon is that
  `--project` bumps that project's `last_used_at`). The session id is read from
  the `session new` output, or looked up by the exact session name; when neither
  works the macOS leg is `macos_provisioning_failed` and the name is printed for
  manual cleanup. The daemon starts that shell in the project checkout whatever `--cwd` says, so every command first runs `cd /tmp/ph-macos-cache` and checks that `pwd -P` is `/private/tmp/ph-macos-cache`; when it is not, the command is not run, nothing more is typed (cleanup included, the roots created so far are listed for manual removal) and the leg is `macos_provisioning_failed`. One function (`mac_send`) is the only place that types into the Mac shell. Removals take only allow-listed absolute paths (`/private/tmp/ph1112*`, `<HOME>/.ph1112` for a `HOME` of the form `/Users/<name>`) of roots this run created, validated before anything is sent. Teardown stops and removes the session on every exit path
  (`cleanup-session-removed`). The pohunek binaries under test talk only to their
  own throwaway daemon, under a throwaway
  `HOME`-relative root and `/private/tmp/ph1112`, with `XDG_RUNTIME_DIR` set so
  the owner's live daemon path is never used. The guard compares the doctor
  paths with the roots before anything starts. Each of the three roots (the
  communication directory, the runtime root and `~/.ph1112`) is created with a
  plain `mkdir`, so an existing one stops the macOS leg (`macos_provisioning_failed`)
  and is never touched; a root is removed at the end only when its own `mkdir`
  succeeded in this run. The throwaway daemon's pid is written by its start
  command; the cleanup sends it SIGTERM, then SIGKILL, only while its command
  line still names the run's binary. Every `brew` call sets
  `HOMEBREW_NO_AUTO_UPDATE`, `HOMEBREW_NO_INSTALL_CLEANUP` and
  `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK`. Tailscale is installed only when
  `brew list` answers rc 1 with no output; any other answer is inconclusive. It is
  uninstalled only when this run installed it and a re-check still lists the
  formula; a re-check that lists nothing means there is nothing to restore. A
  forward that still listens after three removals is left to teardown, which removes
  it and stops VM-A only after the issued-key scan, and the evidence then fails
  `exposure-window-closed`. The session is stopped at the end.

### Teardown and failure

A trap on exit, interrupt and termination removes the port forward, cleans the
Mac, stops the throwaway NetBird daemon, collects the journals and doctor reports
into the state directory, sends `quit` to each monitor, stops a leftover `qemu`
by exact PID, deletes the run directory and lists any remaining process of the
run.

The run has one verdict:

| Verdict | Meaning |
|---------|---------|
| `passed` | Every gating check of every requested leg passed and nothing is inconclusive. |
| `failed` | A gating check failed while its preconditions held (a D4, D6 or NetBird-check step). |
| `incomplete` | A requested leg lacks a recorded step. |
| `inconclusive` | An environment condition stopped the run or one sub-step; no product verdict exists for it. |

Run-wide inconclusive codes: `checksum_mismatch`, `artifact_missing`,
`artifact_unusable` (a CI binary needs a newer glibc than 2.41),
`slots_unavailable`, `provisioning_failed`, `no_direct_path`,
`path_guard_violation`, `interrupted`. Sub-step codes (only the named part has
no verdict): `headscale_unreachable_from_macos`, `macos_userspace_refused` and
`macos_userspace_not_running` void the userspace sub-step; `macos_unresponsive`
(any Mac step timed out), `macos_provisioning_failed`, `macos_homebrew_failed`
and `macos_tailscale_pin_mismatch` void the whole macOS leg. A failed exposure-window or cleanup check is never hidden by these codes (only `macos_unresponsive` voids the cleanup checks).

A daemon that fails `health` (after `service install` on a VM, or the restarted
daemon on the Mac) is a product failure, not an inconclusive condition. The
owner's NetBird peer total must not change during the `netbird` leg; the
connected count is recorded but does not gate.

### Evidence

Schema `pohunek.acceptance.tailscale-overlay`, version 1. Top level: `generated`,
`started`, `legs`, `meta` (the pins, the CI run and head, the SHA-256 of each
binary, the qemu and kernel versions), `verdict`, `inconclusive`, `checks`
(`leg`, `name`, `status` of `passed`, `failed` or `missing`, `detail`, `gating`,
`substep`), `steps` (per leg: `argv`, `rc`, `at`, `stdout`, `stderr` and
`truncated`, each stream cut at 16 KiB) and `recordings`, the redacted real
`tailscale status --json` of a Linux kernel-TUN node and of the macOS userspace
node.

Secrets never reach the document. Node and WireGuard keys, `AuthURL` values and
endpoint lists are redacted; the owner's NetBird addresses become
`<netbird-ip-N>` placeholders in the `netbird` and `macos` legs. Those two legs
run against the owner's machines and the repository is public, so their steps go
through an allow-list projection: a step that is not listed is recorded with its
exit code and size only (stderr is always withheld), host records keep only
`overlay`, `classification`, `local` and `port` with every peer except `zajcaair`
named `<peer-N>` (no FQDN, NetBird domain, address, peer id or daemon version),
doctor reports keep a detail only for the Tailscale and NetBird listener, CLI and
config checks, and the Mac roots and home directory are placeholders
(`<macos-root>`, `<macos-runtime-root>`, `<macos-comm-root>`, `<macos-home>`).
Journals and doctor reports collected into the state directory are scanned for key
prefixes right after collection on every exit path, and a file with a hit is
deleted. `assemble`
refuses to write the document when a `tskey-` or `hskey-` prefix appears
anywhere in the state directory or the document, and when VM-A, which keeps a
copy of every key it issued, finds one of them in the evidence bundle (only the
number of matching files comes back). The interpretation logic is unit-tested in
`scripts/tests/test_overlay_evidence.py`; the script itself is a manual
acceptance tool and is never run in CI.
