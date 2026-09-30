---
type: Guide
id: guide/environment-resolution
title: Environment and executable resolution
description: How pohunek chooses the PATH of the daemon and agents on macOS and how attach commands are rendered safely.
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
   absolute and executable; it is used as is and no search happens.
2. **A profile or service environment `PATH`.** A non-empty
   `[environment] search_path` in `service.toml` is authoritative: no discovery
   runs and no lower tier is consulted. Upgrades preserve it, so it is the
   operator's override point. A fresh install has no `service.toml` yet, so this
   tier takes effect for upgrades and hand edits.
3. **Bounded login-shell discovery (macOS only).** One `$SHELL -l -c` probe
   prints `PATH` between two random sentinel lines through the absolute
   `/usr/bin/printenv`. It is never interactive (`-i` is not used), reads a
   null stdin, starts from an empty environment plus `HOME`, `USER`, `LOGNAME`,
   `TERM=dumb`, and a baseline `PATH`, and runs in its own process group. A hard
   deadline (10 s) kills the group; output above 64 KiB kills it too. User
   startup output before or after the sentinels is ignored, and a decoy line
   cannot spoof the random sentinel.
4. **A fallback directory list.** Used when tier 3 is unavailable or failed.
   The list is one table in `pohunek_platform::shell_env`
   (`DARWIN_FALLBACK_DIRECTORIES`): `~/.local/bin`, `~/.cargo/bin`,
   `~/.bun/bin`, `/opt/homebrew/{bin,sbin}` (Apple Silicon Homebrew),
   `/usr/local/{bin,sbin}` (Intel Homebrew and vendor installers), then the
   system directories. Only existing directories are kept; no single Homebrew
   prefix is assumed.

Every tier yields absolute, normalized, control-character-free, deduplicated
directories. Login-shell output and the fallback table keep existing
directories only, and entries that are empty, relative, `.`-style, duplicated,
or missing are dropped. A `PATH` containing a control character is garbage and
fails the probe.

## Failure handling

Discovery fails closed and never invents a value for required configuration:

| Login shell behaviour | Result |
| --- | --- |
| Hangs past the deadline | killed (process group), `Timeout`, fallback list used |
| Prints more than the output bound | killed, `OutputTooLarge`, fallback list used |
| Prints nothing or junk without the sentinels | `MalformedOutput`, fallback list used |
| Exits non-zero | `Failed`, fallback list used |
| Prints a `PATH` with no usable directory | `UnusablePath`, fallback list used |

The fallback is an optional platform default and its use is logged with the
typed reason (`login shell PATH discovery failed; using the fallback directory
list`). If not even one fallback directory exists, `pohunek service install`
fails with `service_search_path_unavailable` instead of starting a daemon with
an empty search path.

## Where the result goes

`pohunek service install` records the resolved directories in
`service.toml` under `[environment] search_path` and hands their `:`-joined
value to the daemon job as `PATH` (the launchd plist `EnvironmentVariables`).
The daemon forwards `PATH` to workers and agents through the existing
`[environment] allowlist`, which stays the only channel: no other variable is
added, and no secret or interactive environment is captured. Linux and systemd
keep `search_path = []`: the daemon inherits the user manager's environment and
no `PATH` is written.

Upgrades and rollbacks reuse the recorded `search_path` and never run a login
shell. To pick up a newly installed prefix, edit `search_path` in
`service.toml` or reinstall.

## Safety rules

- The login shell runs once, at install time, as the installing user. No user
  file is sourced in a privileged context and no command is ever run through a
  shell except this probe.
- Only one `-c` script is passed, built from a random sentinel and the
  absolute `printenv` path; no untrusted value is interpolated into it.
- Discovery output is bounded, validated, and never logged verbatim; errors
  carry reasons, not the printed value.

## Attach command templates

The GUI attach template uses `{bin}`, `{host}`, and `{id}`. It runs in one of two
modes (`attach_command_mode`), both in `pohunek-gui-core`:

- **`shell`** (default): `render_attach_command` renders one string for `sh -c`.
  Substitution is a single pass, and a placeholder is accepted only where the
  shell reads it as an unquoted word; its value is escaped as exactly one such
  word. A value containing quotes, `$()`, backticks, newlines, `;`, spaces,
  Unicode, or another placeholder therefore never changes the command's
  structure. A POSIX quote state machine finds the positions. A placeholder
  inside `'...'`, `"..."`, `$'...'`, a comment, after a heredoc operator, line
  continuation, or command substitution, or right after a backslash, is refused
  with `AttachTemplateError::UnsafePlaceholderContext` (an unclosed quote is
  `UnterminatedQuote`). To run a nested script, pass the values as positional
  parameters instead of quoting them into the script:
  `attach_command = "$TERMINAL -e sh -c 'exec \"$@\"' sh {bin} attach --host {host} {id}"`.
- **`argv`**: `render_attach_argv` renders an argument vector without a shell.
  Only the template is split (POSIX quoting, no expansion, only space, tab, and
  newline separate words); values are inserted after splitting as data in
  exactly one argument each, so a quoted placeholder such as
  `terminal -- "{bin}"` is fine and a path with spaces stays one argument.
  This is the recommended mode for any launcher that needs no shell features.

The GUI validates the template at config load, so a refused template fails at
startup instead of at the first attach.
