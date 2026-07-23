---
type: Guide
id: guide/web-control-center
title: Web control center
description: Run and understand the optional browser client, its backend origin, and its TypeScript package surfaces.
source_kind: manual
intents: [setup, update, debug, help]
---

# Web Control Center

The optional web control center is a client surface over the existing public
protocol. One `@pohunek/backend` origin serves the Svelte SPA, reports hosts at
`GET /api/hosts`, and exposes a transparent control or attach WebSocket per
daemon. The backend is not authoritative: each daemon still owns its sessions,
PTYs, events, and notifications, and the CLI and native GUI keep working when
the backend is down.

The control center uses one persistent, session-first workspace shell. The
session rail combines every host, groups normal work by project, and promotes
blocked sessions into an Attention section. Search covers session, project,
repository, branch, agent, and host fields; filters narrow activity or finished
work. Host connectivity remains visible in a compact strip, but hosts are
context rather than the primary navigation level.

Selecting a running session attaches its PTY directly in the main pane without
hiding the rail. Switching sessions detaches the old view and attaches the new
one; resize and binary terminal traffic still use the daemon-owned attach
stream. Non-running and observe-only sessions show a summary instead. The
terminal toolbar can rename, stop, resume, fork, or permanently remove eligible
sessions, while the inspector edits individual metadata keys. External observed
sessions never expose mutating controls. Removal always requires confirmation
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
  serving, and unchanged 1:1 WebSocket relay framing.
- `@pohunek/client-core`: framework-free multi-host state and actions used by
  the SPA.
- `@pohunek/frontend`: the Svelte control-center SPA.
- `@pohunek/testkit`: the stateful fixture daemon used by tests and dev mode.

For development, run `bun run dev` from `web/`. It starts two loopback fixture
daemons, the backend with its explicit loopback-development allowance, and the
Vite frontend. It needs neither a Rust daemon nor NetBird. Bun remains the
workspace runtime and orchestrates the fixture daemons and backend. The command
also requires `node` on `PATH`: Vite runs in a managed Node child process because
Vite 8's WebSocket proxy relies on Node `net.Socket` APIs that Bun 1.3 does not
provide. Set `POHUNEK_NODE_BIN` when Node has a nonstandard executable path.
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
