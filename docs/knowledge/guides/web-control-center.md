---
type: Guide
id: guide/web-control-center
title: Web control center
description: Run and understand the optional browser client, its backend origin, and its TypeScript package surfaces.
source_kind: manual
intents: [setup, update, debug, help]
---

# Web Control Center

## Current shipped control center

The optional web control center is a client surface over the existing public
protocol. One `@pohunek/backend` origin serves the Svelte SPA, reports hosts at
`GET /api/hosts`, and exposes a transparent control or attach WebSocket per
daemon. The backend is not authoritative: each daemon still owns its logical
sessions, events, and notifications, while per-session workers own live PTYs.
The CLI and native GUI keep working when the backend is down. This is a
mesh-local owner tool, not the accepted public team relay.

The backend remains a transparent owner transport for the additive
`host.governance.inspect` method. The generated TypeScript method map exposes
the safe result to a caller that asks for it, but the current owner WebUI does
not add a governance screen, team mode, relay-local mode, or an authority
fallback. Existing create, attach, detach, and stop flows keep their unchanged
owner behavior.

The control center uses one persistent, session-first workspace shell. The
session rail combines every host, groups normal work by project, and promotes
blocked sessions into an Attention section. Search covers session, project,
repository, branch, agent, and host fields; filters narrow activity or finished
work. Host connectivity remains visible in a compact strip, but hosts are
context rather than the primary navigation level.

Selecting a running session attaches its PTY directly in the main pane without
hiding the rail. Switching sessions detaches the old view and attaches the new
one; resize and binary terminal traffic still use the daemon attach proxy.
After daemon replacement the browser must compare `runtime_id`: the same
generation may reconnect and repaint, while a changed generation is explicit
native recovery. Lost, conflicting, incompatible, and observe-only sessions
show a summary instead. The
terminal toolbar can rename, stop, resume, fork, or permanently remove eligible
sessions, while the inspector edits individual metadata keys. External observed
sessions never expose mutating controls. When provider lifecycle hooks have
reported Claude/Codex subagents, a durable status strip above the terminal shows
their type and running or terminal lifecycle. Removal always requires confirmation
and warns when it will also stop a live PTY. Session creation is a modal that
measures terminal geometry invisibly and attaches after creation, and the Inbox
is an unread-first drawer. Opening a session-backed notification marks it read
and selects that session. A failed peer remains marked as an error without
disabling reachable hosts.

The host-scoped Projects screen registers a repository using an explicit
absolute path on the daemon host, lists and renames project records, and removes
them with an optional Pohunek-owned worktree prune. Project detail shows the
live worktrees and links their active sessions. Only an owned worktree without a
live session exposes removal, and the browser still relies on the daemon's typed
ownership and lifecycle safeguards as the final authority.

Keyboard controls apply only outside inputs, editable content, and the embedded
terminal. `Ctrl+K` opens the command palette, `Ctrl+B` toggles the rail, `n`
opens session creation, `i` opens the Inbox, `b` cycles blocked sessions, `/`
focuses search, and `j`/`k` or the arrow keys move focus through session rows.
`Enter` activates the focused row and `Esc` closes the active overlay. The last
valid session selection and rail state persist locally; a selection that is not
present after the initial session snapshots settle is discarded.

Agent presentation is capability- and inventory-driven. The creation dialog can
show Hermes and Hermes-based profiles only when the selected host reports a
known `agent_base` and a supported runtime; an unavailable, unsupported, or
unknown future base is display-only. Lifecycle controls use each session's
advertised resume/fork capabilities, so Hermes shows resume only when the
session can recover and never offers fork.

On mobile and short touch viewports, the rail is an accessible off-canvas
drawer and the terminal expands to the available visual viewport. A touch
toolbar focuses the software keyboard and sends Escape, Tab, Control-C, arrow
keys, or one-shot Control and Alt combinations directly to the attached PTY.
The layout uses full-viewport overlays, 44-pixel touch targets, safe-area
insets, and portrait and landscape breakpoints. TLS and installable PWA support,
as well as provider integration, remain later milestones.

The TypeScript surfaces are:

- `@pohunek/protocol`: types generated from the Rust protocol source.
- `@pohunek/sdk`: the shared runtime plus Bun/Node Unix and TCP transports.
- `@pohunek/sdk/browser`: the browser-safe entry with only the WebSocket path;
  it contains no `node:net` dependency.
- `@pohunek/backend`: local-daemon host discovery, `/api/hosts`, static SPA
  serving, and unchanged transparent 1:1 WebSocket framing.
- `@pohunek/client-core`: framework-free multi-host state and actions used by
  the SPA.
- `@pohunek/frontend`: the Svelte control-center SPA.
- `@pohunek/testkit`: the stateful fixture daemon used by tests and dev mode.

For development, run `bun run dev` from `web/`. It starts two loopback fixture
daemons, the backend with its explicit loopback-development allowance, and the
Vite frontend. It needs neither a Rust daemon nor NetBird. Bun remains the
workspace runtime and orchestrates the fixture daemons and backend. The command
also requires Node: Vite runs in a managed Node child process because
Vite 8's WebSocket proxy relies on Node `net.Socket` APIs that Bun 1.3 does not
provide. The orchestrator finds Node itself (see "Runtime paths, logs and macOS");
set `POHUNEK_NODE_BIN` to an absolute path to override.
Structured output is also written under the gitignored `web/logs/` directory.

A deployed backend requires its local `pohunekd` for health and host discovery
and fails startup when that daemon is unreachable. It binds only to a NetBird
CGNAT address; loopback is allowed only by the explicit development flag, and
wildcard binds are rejected. Use the supplied
`web/backend/systemd/pohunek-backend.service` user unit and keep
`~/.config/pohunek/backend.env` owner-only. The environment file must set
`POHUNEK_BACKEND_BIND_HOST` and `POHUNEK_BACKEND_PORT`; it can override the
local daemon socket with `POHUNEK_BACKEND_DAEMON_SOCKET`.

For a released Linux x86_64 deployment, download the
`pohunek-web-*-linux-x86_64.tar.gz` release asset, unpack it, and run its
`install.sh`. The archive contains a standalone backend executable with Bun
embedded and the compiled SPA, so the target host does not need a Bun or source
checkout. The installer writes a systemd user unit under the current user's XDG
config directory and preserves an existing `backend.env`; edit that file with
the host's NetBird address and chosen port, then run `systemctl --user
daemon-reload` and `systemctl --user enable --now pohunek-backend.service`.
The backend still runs on the same host as `pohunekd`, keeping Unix-socket
discovery and the NetBird-only bind boundary intact.

Browser code imports `Client` from `@pohunek/sdk/browser` and calls
`Client.connectWs(window.location.origin, host)`. It must not dial daemon TCP or
Unix sockets directly. The backend only tunnels the public newline-delimited
JSON control frames and raw attach bytes; it does not define a second protocol.

## Runtime paths, logs and macOS

The backend resolves the daemon socket with the same contract as the Rust
host components (`crates/paths/fixtures/runtime-paths.json` drives both
implementations):

- An explicit, valid absolute `XDG_RUNTIME_DIR` selects
  `$XDG_RUNTIME_DIR/pohunek/daemon.sock` on Linux and macOS. An empty,
  relative, parent-component or NUL-containing value is a configuration error,
  never treated as absent.
- Without it, Linux fails fast and macOS uses
  `/private/tmp/pohunek-<effective-uid>/daemon.sock`; `TMPDIR` plays no role.
- `POHUNEK_BACKEND_DAEMON_SOCKET` overrides both and is checked by the same
  rules: absolute, no parent component, no NUL, and within the platform's
  socket path limit (103 bytes on macOS, 107 on Linux).

A socket path over the limit fails at startup with the variable that caused it
instead of failing inside the connect call. When the socket is derived (not
overridden), the backend also refuses to start unless the runtime directory
exists and is a real directory, without symlinked components, owned by the
current user with mode exactly `0700`, and a present socket is a socket of the
same user: the macOS default lives under the shared `/private/tmp`, where
another local user could pre-create the predictable path.

By default the backend writes one JSON object per line to standard output
(journald keeps it under systemd). A launchd job has no journal, so setting
`POHUNEK_BACKEND_LOG_DIR` makes the backend write an owner-private (`0700`
directory, `0600` file) rotating family `pohunek-backend.jsonl[.N]` instead.
`POHUNEK_BACKEND_LOG_MAX_FILE_BYTES` (default 32 MiB) and
`POHUNEK_BACKEND_LOG_MAX_FILES` (default 8, including the active file) bound it
like the daemon's own log family; the two limits are rejected without a log
directory, and a file limit too small to hold the fixed oversize notice stops
startup. A symlinked directory or active file, a non-regular file such as a
FIFO in any log slot, a directory open to group or others, or a foreign owner
stops startup; a symlink in a rotated slot is removed without being followed.
Files left by an earlier run are brought inside
the bound when the backend starts: oversize ones are removed (the active one is
emptied) and loose modes are forced to `0600`. An event larger than one file is
replaced by a fixed notice, so total disk use stays within the product of the two
limits. A failing write or rotation (a full disk, an I/O error) never fails a
request: the event goes to standard output, one `log_file_failed` event reports
it, and the next event tries the files again. A partly written line is cut back
off; if that fails, the next event starts a fresh active file. A failed start is
recorded in the files as `backend_startup` `failed` before they are closed. The
backend closes the files with its own shutdown.

The backend runs natively on Apple Silicon, both from the Bun workspace and as
the compiled release executable. `bun run dev` locates Node itself: it uses
`POHUNEK_NODE_BIN` when set (an absolute executable file), else the first
executable `node` on `PATH`, else the Homebrew and package-installer prefixes
on macOS, and names both the search and the override when none is found.

## Separate accepted team web surface

The [optional team-relay design](../concepts/team-relay.md) has an implemented
foundation for generic OIDC authentication and bounded account and credential
lifecycle. It has no team browser client or team WebUI. The future Rust
`pohunek-relayd` will extend that foundation into the single team-path
authorization, routing, aggregation, and browser-API authority, and will serve
a team-mode SPA. Team browser clients will use a typed relay API rather than
forwarding arbitrary daemon NDJSON through a transparent tunnel.

The current `@pohunek/backend` runtime and the release/install instructions
above remain supported after
[#86](https://github.com/zajca/pohunek/issues/86) delivers that separate team
surface. `pohunek-relayd` has no local mode. Owner and team WebUIs may share
Svelte presentation components, but use separate explicit origins, API
adapters, credentials, and state; neither silently falls back to the other.
The Bun backend never becomes a team authorization or relay-routing authority.
The current foundation's native credential commands and bounded HTTPS endpoints
do not create a team browser surface; [#86](https://github.com/zajca/pohunek/issues/86)
owns that client.

## macOS archive and launchd agent

The release workflow also builds `pohunek-web-<version>-aarch64-apple-darwin`
(the compiled backend with Bun embedded, the SPA, `install.sh`, and
`packaging/verify-archive`) when the macOS signing credentials exist; macOS is
not yet a published platform. Its `install.sh` verifies the archive `MANIFEST`
(component, target, macOS 14 minimum, member digests), requires an installed
daemon, and registers a launchd login agent
`io.github.zajca.pohunek.<daemon namespace>.backend`, a separate client service:
installing, updating (the backend restarts, nothing else), or removing it
(`./install.sh --uninstall`, which keeps `backend.env` and the logs) never
touches the daemon or a session. The agent's `ProgramArguments` is the installed
backend path alone, the property list is written by `plutil`, and its
environment holds only the non-secret `POHUNEK_BACKEND_*` settings copied from
`backend.env` (an allowlist; the static and log directories belong to the
installer), the log directory, and `XDG_RUNTIME_DIR` when set. launchd has no
environment file, so the agent is registered only once the bind host and port are
set, and `./install.sh` is re-run after each edit of `backend.env`. Logs go to the
bounded owner-private `~/.local/state/pohunek/web-logs/pohunek-backend.jsonl`
family; a failure before the configuration loads lands in `launchd.stderr` in the
same directory. The signed executable carries the JIT entitlements Bun documents
for standalone executables.

