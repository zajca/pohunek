---
type: Guide
id: guide/environment-resolution
title: Environment and executable resolution
description: How pohunek chooses the PATH of the daemon and agents on macOS.
source_kind: manual
intents: [setup, debug, help]
---

# Environment And Executable Resolution

Processes started by launchd or from Finder receive a minimal `PATH`
(`/usr/bin:/bin:/usr/sbin:/sbin`). Agents installed with Homebrew, `cargo`,
`bun`, or `~/.local/bin` are invisible to them. Pohunek resolves the search path
once, at `pohunek service install`, with one documented policy, and records the
result so every later start is deterministic.

## Resolution order

Highest priority first:

1. **A configured absolute executable.** A program name containing `/` must be
   absolute and executable; a relative one (`./agent`, `bin/agent`) is refused
   and no search happens. The daemon resolves agent programs (capability
   detection, create, resume) and the helper programs it runs itself (`git`, the
   worktree-hook interpreter `sh`) through this same resolver and spawns the
   pinned path; the doctor's `bin:` checks, the capability snapshot (`git_available`,
   worktree support, agent runtimes), and the CLI's daemon lookup use the same
   resolver, so all of them agree on what would run, skipping empty and
   relative `PATH` entries (a trailing colon is harmless) rather than failing, and launches the exact
   canonical path it probed.
2. **An explicitly supplied environment `PATH`.** A caller that already has a
   validated `PATH` (a client launched from a shell passes its inherited one)
   uses it without discovery. `pohunek service install` supplies none.
3. **Bounded login-shell discovery (macOS only).** One `$SHELL -l -c` probe
   prints `PATH` between two random sentinel lines through the absolute
   `/usr/bin/printenv`. It is never interactive (`-i` is not used), reads a
   null stdin, starts from an empty environment plus `HOME`, `USER`, `LOGNAME`, `SHELL` (the shell
   being probed, so a profile that branches on it behaves as in a real login), the
   profile selectors `ZDOTDIR` and `XDG_CONFIG_HOME` when set (each must be an
   absolute UTF-8 path, or the install fails; a client that skips the probe names
   the cause), built by the one shared `shell_env::login_environment`,
   `TERM=dumb`, and a baseline
   `PATH`, and runs in its own process group. One
   deadline (10 s) covers the fallback-directory validation, the executable
   checks, the probe, and the validation of the printed directories; output above 64 KiB kills the probe too. The
   group is killed as soon as the shell exits, so a background job left by a
   startup file cannot hold the output open. User startup output before or
   after the sentinels is ignored (the first sentinel starts on its own line, so
   a banner without a trailing newline cannot glue to it), and a decoy line cannot spoof the random
   sentinel. A login shell reads profile files but not `.zshrc`, so the trusted
   fallback directories it lacks are appended after the discovered ones.
4. **A fallback directory list.** Used alone when tier 3 is unavailable or
   failed. The list is one table in `pohunek_platform::shell_env`
   (`DARWIN_FALLBACK_DIRECTORIES`): `~/.local/bin`, `~/.cargo/bin`,
   `~/.bun/bin`, `/opt/homebrew/{bin,sbin}` (Apple Silicon Homebrew),
   `/usr/local/{bin,sbin}` (Intel Homebrew and vendor installers), then the
   system directories. No single Homebrew prefix is assumed.

An executable found by any tier (and the login shell itself) is judged on the
opened file: symlinks are resolved, then the final file must be a regular file
owned by the user or root, writable by neither group nor others, and without an
ACL that grants access (macOS), and the kernel must agree the user can run it.
A candidate that fails is skipped and the search goes on, like one the shell
cannot execute. The path is returned as found, not canonicalized, so a
multi-call binary (`sh` linked to `bash` or `busybox`) keeps the name it was
started by. The directories the lookup passes through must also be
owner-controlled, on the lexical chain and on the canonical chain of a symlink
target: every component passes the executable-lookup ancestor policy (user-owned
components not group- or world-writable, root-owned ones non-writable or
sticky; the macOS admin-group exception below), every symlink on the chain is owned by the user or root, and a
directory others can write (a sticky `/tmp`) may hold a chain entry only when
the entry is ours or root's, so no other account can rename or retarget an
entry after the check. A
path-based exec cannot be made atomic with the check; the chain check removes
the attacker's ability to change what the path names. A private 0700 directory
below the sticky root-owned `/tmp` passes. The executable file itself is never
accepted when group- or world-writable.

**macOS admin-group exception.** A lookup directory may be group-writable on
macOS when the process does not run as root, it is owned by the user, its group is `admin` (gid 80), it is not
writable by others, and it has no allow ACL. This is the shape of a stock
Homebrew `bin` and `Cellar` (`user:admin 775`). A process running as root never applies the exception, because any admin could
then plant a binary that root runs. Root-owned directories that
the admin group can write (`/Applications`, an MDM-managed Homebrew) stay
refused, as does any other group, so the app-bundle command-line tools stay
unsupported. The rule lives only in the executable lookup; every other trusted
directory check (stores, locks, package roots) keeps the strict policy. The
canonical target of a Homebrew link is not pinned, because the keg path crosses
`Cellar`, which the group can write as well. Intel Homebrew (`/usr/local/bin`
owned by the user, group `admin`) follows the same rule but is not verified:
Intel Macs are outside the release scope.

*Residual risk.* A process running as another admin account of the same Mac can
rename or swap entries that the user or root owns in the user's Homebrew
directories, or hard-link a file the user or root owns (from the same volume)
into them, and Pohunek then runs them as the user; no password is needed.
Entries that account creates are owned by it and stay refused. On a Mac where
the user is the only admin this adds no principal; `dscl . -read /Groups/admin
GroupMembership` also lists `root` and the macOS Setup Assistant account
`_mbsetupuser`, and neither is another admin account in this sense. A host with several admins
removes the exposure with `chmod g-w` on the Homebrew directories (`bin`,
`sbin`, `Cellar` and the others the prefix lists as group-writable): the strict
rule then accepts them and Homebrew works as a single-user install.

A bare name that
finds no trusted executable but finds a candidate that failed a check reports
the first such candidate and the check (`found <path> but not trusted:
<reason>`); a name found nowhere reports that none exists. On macOS a directory
dropped from the recorded `PATH` says which write right made it unsafe: "writable
by others", "writable by a group other than admin (gid 80)", or "writable by the
admin group (gid 80), accepted only in a directory the effective user owns and
never for root" (a directory of another owner, or any directory when the process
runs as root). A refused configured
executable reports which check failed (`Refusal`: owner uid, mode, ACL, the
untrusted chain directory, or the errno of the open or execute check) so a
rejection is diagnosable. On macOS an ACL counts only when an allow entry
grants a change right (deny entries such as `everyone deny delete` do not). An
ACL that cannot be determined (any query error other than "no ACL") is not
trusted: the candidate is refused with the errno.

Login-shell output and the fallback table are untrusted input, so a directory
is kept only when it is trusted: symlinks are resolved, every component of the
canonical path is owned by the user or root and not writable by others (the
platform's trusted-ancestor rules), and the directory itself is not writable
by group or others. `/tmp`, other sticky world-writable directories, anything
below a writable ancestor, and group-writable directories (except the macOS
admin-group exception above) are refused, because another local account could
plant an agent executable there. Each kept entry is recorded as its validated canonical path, so validation
and recording cover the same directory and nothing can be retargeted after the
check. The trade-off: a profile symlink that later points elsewhere (nix
generations, a dotfile manager) keeps the recorded target until the path is
recorded again with `pohunek service refresh-path`, which also restarts the
daemon with it. An install made before the admin-group exception keeps a
recorded `PATH` without `/opt/homebrew/bin` until that command runs; Homebrew
`git` then also shadows `/usr/bin/git` when it comes first, as in the user's
shell. An upgrade refused as at risk
points to it (see the
[upgrade runbook](../runbooks/update-after-release.md#a-recorded-search-path-that-no-longer-matches-your-profile)). Empty, relative, `.`-style, duplicate, and
missing entries are ignored; a `PATH` containing a control character is
garbage and fails the probe.

## Failure handling

Discovery fails closed and never invents a value for required configuration:

| Login shell behaviour | Result |
| --- | --- |
| Hangs past the deadline | killed (process group), `Timeout`, fallback list used |
| Prints more than the output bound | killed, `OutputTooLarge`, fallback list used |
| Prints nothing or junk without the sentinels | `MalformedOutput`, fallback list used |
| Exits non-zero | `Failed`, fallback list used |
| Prints a `PATH` with no usable directory | `UnusablePath`, fallback list used |
| `$SHELL` is set but not absolute | `pohunek service install` fails (`service_environment_invalid`) |
| `$SHELL` is unset | `/bin/zsh` is used and the report says so |

The discovery environment is validated, and discovery run, before any install
effect, including before an interrupted install of another version is rolled
back (rolling back an interrupted upgrade needs no discovery), and
`pohunek service check` runs the same validation. If not even one trusted fallback directory exists, `pohunek service install`
fails with `service_search_path_unavailable` instead of starting a daemon with
an empty search path. A `HOME`, `USER`, or `LOGNAME` that is set but not UTF-8
fails the install rather than being left out of the probe environment.

## Where the result goes

`pohunek service install` records the resolved directories in `service.toml`
under `[environment] search_path` and hands their `:`-joined value to the daemon
job as `PATH` (the launchd plist `EnvironmentVariables`). The daemon forwards
`PATH` to workers and agents through the existing `[environment] allowlist`,
which stays the only channel: no other variable is added, and no secret or
interactive environment is captured. Linux and systemd keep `search_path = []`:
the daemon inherits the user manager's environment and no `PATH` is written. The
install, upgrade and rollback judge that job with the `PATH` the unit really gets: the
user manager's, replaced by an `ExecSearchPath=` (joined with `:`) and then by an `Environment=PATH=` of a drop-in of the daemon unit (the install replaces the unit file itself, so a `PATH` set only there does not count). Where the unit has no file yet, or a rollback removed it, its drop-in files are read from disk in file-name order across directories (a higher-priority directory wins a shared name, a `/dev/null` link disables it, hidden and backup files are skipped).
A unit setting the CLI cannot read (an `EnvironmentFile=` that sets `PATH`,
`PassEnvironment=PATH`, `UnsetEnvironment=PATH`) is refused with the file or directive to
change, instead of judging a `PATH` the job does not get.

The install result reports how the path was obtained: `search_path.source`
(`login_shell`, `fallback`, `recorded` for a resumed install, or `unmanaged`),
the recorded `entries`, `shell_used` and `shell_defaulted`, the typed
`login_shell_failure` when the fallback list was used, and `dropped`, the
directories refused as untrusted with a reason. The human output prints a
`warning:` line for a failed login shell and for each refused directory. This
is the only place the outcome is visible, so read it after installing.

The path is recorded at install. The job definition is written at install, at
an upgrade to a different version, and by `pohunek service refresh-path`;
upgrades reuse the recorded list without running a login shell, so an edit of
`search_path` in `service.toml` reaches the daemon job only at the next
version-changing upgrade; a same-version upgrade returns before it replaces the
job. To pick up a newly installed prefix, or a profile link that moved, run
`pohunek service refresh-path`: it runs the login-shell discovery again (before
it takes the transaction lock), prints the directories it adds and removes,
rewrites the recorded `search_path` in `service.toml` (atomically, under the
lock) and replaces the daemon job with the new definition in one transaction,
so the running daemon gets the new `PATH`. Only the daemon restarts; worker
jobs keep running. The new daemon's adoption verdict is taken first, as for an
upgrade, and `--accept-runtime-loss` accepts what it lists. `--dry-run` previews
the change and the verdict without a lock. `service check` and `service
upgrade` judge with the new path at once. A login shell that fails, hangs or
prints junk is refused (`service_search_path_refresh_failed`): the fallback list
an install would record then never replaces a recorded path.

## The NetBird CLI

The `netbird` CLI that overlay discovery and `pohunek doctor` run is resolved by
the same trusted-executable policy, in the process that runs it. The lookup
uses the process `PATH` first. On macOS a process launched from Finder or launchd
carries a minimal `PATH`, so a miss continues in the fixed fallback directories
(Homebrew and package-installer prefixes among them); only trusted directories
and executables count. The resolved absolute path is what runs, so the kernel
does no second search. A `netbird` that fails the policy is reported as missing,
and `netbird_cli` in the doctor agrees with the daemon's own probe.
The nix-darwin system profile under `/run/current-system/sw/bin` is accepted
for `netbird` when its root-owned link traverses macOS's trusted system run
directory. The resolved command uses the verified canonical target.

## Safety rules

- The login shell runs once, at install time, as the installing user. No user
  file is sourced in a privileged context and no command is ever run through a
  shell except this probe.
- Only one `-c` script is passed, built from a random sentinel and the
  absolute `printenv` path; no untrusted value is interpolated into it.
- Discovery output is bounded, validated, and never logged verbatim; errors
  carry reasons, not the printed value.
- An abandoned discovery (deadline passed) starts no shell and validates no
  further output; a filesystem call already stuck in the kernel cannot be
  cancelled, only abandoned.

## The daemon job's own environment

The `PATH` recorded at install is the one the unit file (Linux) or property list
(macOS) gives the daemon job, but the service manager can change what the job
really gets, and the upgrade preflight and the rollback gate judge with what the
job gets:

- **Linux.** A drop-in of the daemon unit (`~/.config/systemd/user/<unit>.d/*.conf`)
  applies after the unit file. A drop-in `Environment=PATH=` replaces the recorded
  `PATH` (a warning names it), and a drop-in `Environment=` of an allowlisted store
  variable such as `CLAUDE_CONFIG_DIR` replaces the user manager's value. Setting,
  resetting or removing a bootstrap key (`HOME`, `XDG_*`), an `EnvironmentFile=`
  that assigns a judged variable and an unreadable unit are refused with
  `service_manager_environment_unavailable`; the message names the file or
  directive. An unreachable user manager refuses a recorded `PATH` too.
- **macOS.** A job that is not loaded and has no journaled default takes the
  persisted launchd configuration only when `user.plist` is older than the boot;
  a newer one may be pending, so a forward gate is refused and a rollback goes on
  with a warning.

The rules are in the
[upgrade runbook](../runbooks/update-after-release.md#the-journal-of-an-interrupted-upgrade).
