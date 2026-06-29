---
type: Guide
id: guide/gui
title: GUI setup
description: Configure and troubleshoot the native pohunek-gui desktop control plane.
source_kind: manual
intents: [setup, debug, help]
---

# GUI Setup

`pohunek-gui` is the native desktop control plane. It lists hosts, sessions,
projects, worktrees, and agent state through the Rust SDK. It does not embed a
terminal: opening a session delegates to the user's terminal by spawning the
configured `attach_command`.

Use this guide when the user asks to configure or debug the GUI.

## Preconditions

Start with the normal local setup checks:

1. Run `pohunek doctor --json`.
2. Run `pohunek daemon start --detach` if the daemon is not running.
3. Run `pohunek health --json` or `pohunek status --json`.
4. Run `pohunek host inspect local --json` to confirm agent and worktree
   capabilities.

The GUI also needs a graphical session. On Linux v1 that means a Wayland session
with a reachable compositor. If `pohunek-gui` fails with a Wayland
`NoCompositor` error, the process cannot see the user's compositor socket; run it
from the user's normal desktop shell instead of a restricted sandbox.

## Configuration File

The GUI reads `gui.toml` from the shared Pohunek config directory:

- `$XDG_CONFIG_HOME/pohunek/gui.toml` when `XDG_CONFIG_HOME` is set.
- `~/.config/pohunek/gui.toml` otherwise.

Minimal local configuration:

```toml
pohunek_bin = "/path/to/pohunek"
attach_command = "$TERMINAL -e sh -c 'exec {bin} attach --host {host} {id}'"

[gui]
connect_timeout_ms = 2000
request_timeout_ms = 5000
reconcile_secs = 30
backoff_initial_ms = 1000
backoff_max_ms = 30000
```

Use an absolute `pohunek_bin` when the GUI may be launched from a desktop
environment with a different `PATH`. For a source checkout, build first and point
at `target/debug/pohunek` or `target/release/pohunek`.

The `attach_command` template supports exactly these placeholders:

- `{bin}`: the configured Pohunek CLI binary.
- `{host}`: the host value to pass to `pohunek attach`; empty for the local
  daemon.
- `{id}`: the selected session id.

Keep attach delegation external. Do not configure or recommend an embedded
terminal path; that is intentionally out of scope for the GUI.

## Running

From the repository during development:

```sh
cargo run -p pohunek-gui
```

From an installed build:

```sh
pohunek-gui
```

If the GUI starts but shows no sessions, verify daemon health and `session.list`
first. If host discovery fails, the GUI should still try the local host and
surface a per-host error instead of treating the whole app as failed.

## Project And Worktree Management

The GUI must use existing daemon methods:

- `project.list`
- `project.add`
- `project.show`
- `project.rename`
- `project.remove`
- `session.new`
- `session.inspect`
- `session.stop`
- `session.set_metadata`

Worktree creation is represented by `session.new` with a project or repo and a
branch. There is no standalone worktree daemon method. When explaining or fixing
GUI worktree behavior, preserve that protocol boundary.

## Secrets

Do not put token values in `gui.toml`, session metadata, prompts, snapshots, or
logs. GUI provider configuration should use references such as keyring entry
names. For Linear, the intended GUI shape is a key name like
`linear.token_key`, not the token value.

If a user asks the assistant to edit GUI configuration, inspect or write only
non-secret config values. Never read `.env`, keyring contents, credentials, or
tokens to make the GUI start.

## Source Verification

When behavior must be checked against implementation, inspect:

- `crates/gui/src/main.rs` for config loading, attach spawning, and Iced shell
  behavior.
- `crates/gui-core/src/lib.rs` for headless state transitions, SDK requests, and
  attach command rendering.
- `docs/phases/06-native-app.md` for Track D milestone scope and constraints.
