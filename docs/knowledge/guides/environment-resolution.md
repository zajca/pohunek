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
   absolute and executable; a relative one (`./agent`, `bin/agent`) is refused
   and no search happens. The daemon resolves agent programs (capability
   detection, create, resume) through this same resolver, skipping empty and
   relative `PATH` entries (a trailing colon is harmless) rather than failing, and launches the exact
   canonical path it probed.
2. **An explicitly supplied environment `PATH`.** A caller that already has a
   validated `PATH` (the GUI, launched from a shell, passes its inherited one)
   uses it without discovery. `pohunek service install` supplies none.
3. **Bounded login-shell discovery (macOS only).** One `$SHELL -l -c` probe
   prints `PATH` between two random sentinel lines through the absolute
   `/usr/bin/printenv`. It is never interactive (`-i` is not used), reads a
   null stdin, starts from an empty environment plus `HOME`, `USER`, `LOGNAME`, the
   profile selectors `ZDOTDIR` and `XDG_CONFIG_HOME` when set (each must be an
   absolute UTF-8 path, or the install fails), `TERM=dumb`, and a baseline
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
started by. Group-writable executables, such as an admin-group Intel Homebrew,
are out of scope.

Login-shell output and the fallback table are untrusted input, so a directory
is kept only when it is trusted: symlinks are resolved, every component of the
canonical path is owned by the user or root and not writable by others (the
platform's trusted-ancestor rules), and the directory itself is not writable
by group or others. `/tmp`, other sticky world-writable directories, anything
below a writable ancestor, and group-writable directories (such as an
admin-group Intel `/usr/local/bin`, which is out of scope) are refused,
because another local account could plant an agent executable there. Each kept entry is recorded as its validated canonical path, so validation
and recording cover the same directory and nothing can be retargeted after the
check. The trade-off: a profile symlink that later points elsewhere (nix
generations, a dotfile manager) keeps the recorded target until the path is
recorded again; #319 tracks the refresh command. Empty, relative, `.`-style, duplicate, and
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
effect, including before a foreign pending transaction is rolled back, and
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
the daemon inherits the user manager's environment and no `PATH` is written.

The install result reports how the path was obtained: `search_path.source`
(`login_shell`, `fallback`, `recorded` for a resumed install, or `unmanaged`),
the recorded `entries`, `shell_used` and `shell_defaulted`, the typed
`login_shell_failure` when the fallback list was used, and `dropped`, the
directories refused as untrusted with a reason. The human output prints a
`warning:` line for a failed login shell and for each refused directory. This
is the only place the outcome is visible, so read it after installing.

The path is recorded at install. The job definition is written only at install
and at an upgrade to a different version, and upgrades reuse the recorded list
without running a login shell, so an edit of `search_path` in `service.toml`
takes effect only at the next version-changing upgrade; a same-version upgrade
returns before it replaces the job. To pick up a newly installed prefix,
uninstall and install again. A command that refreshes the path in place is
tracked in #319.

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

## Attach command templates

The GUI attach template uses `{bin}`, `{host}`, and `{id}`. It can be rendered in
one of two ways, both in `pohunek-gui-core`:

- **Shell string**: `render_attach_command` renders one string for `sh -c`.
  Substitution is a single pass, and a placeholder is accepted only as an
  unquoted word; its value is escaped as exactly one such word, so a value
  containing quotes, `$()`, backticks, newlines, `;`, spaces, Unicode, or
  another placeholder never changes the command's structure. The template is
  not parsed as shell. When it holds a placeholder it must fit an allowlist
  grammar: outside single quotes, no backtick, parenthesis, bracket, `<`, `>`,
  literal brace, `#` comment, line continuation, or `$` other than a plain
  `$NAME`; double-quoted text may hold no `$` construct or backtick;
  single-quoted text is opaque; a word that holds a placeholder may not also
  hold an unquoted `*`, `?`, or `~` (quote the literal part, or put it in
  another word). Anything else, a placeholder inside quotes, or a
  placeholder right after `$`, is refused with
  `AttachTemplateError::UnsafePlaceholderContext` (an unclosed quote is
  `UnterminatedQuote`, a template with no command `EmptyCommand`). Values are
  data for the launched program: a shell builtin that evaluates its arguments
  (`let`, `eval`, arithmetic) can still interpret one, so never pass a value to
  such a builtin. To run a nested script, pass the values as positional
  parameters instead of quoting them into the script:
  `attach_command = "$TERMINAL -e sh -c 'exec \"$@\"' sh {bin} attach --host {host} {id}"`.
  Bare words exclude `,` and `=` and a leading `-`, so values never take
  brace-expansion, assignment-word, or option shapes.
- **Argument vector**: `render_attach_argv` renders an argument vector without a shell.
  Only the template is split (POSIX quoting, no expansion, only space, tab, and
  newline separate words); values are inserted after splitting as data in
  exactly one argument each, so a quoted placeholder such as
  `terminal -- "{bin}"` is fine and a path with spaces stays one argument.
  This is the recommended mode for any launcher that needs no shell features.

The GUI validates the template at config load, so a refused template fails at
startup instead of at the first attach.
