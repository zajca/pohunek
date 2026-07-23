<p align="center">
  <img src="assets/pohunek_github_hero.png" alt="pohunek — multihost management daemon for coding agents. I herd. They code." />
</p>

<p align="center">
  <a href="https://github.com/zajca/pohunek/actions/workflows/ci.yml"><img src="https://github.com/zajca/pohunek/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="https://github.com/zajca/pohunek/releases/latest"><img src="https://img.shields.io/github/v/release/zajca/pohunek" alt="Latest release" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT license" /></a>
  <img src="https://img.shields.io/badge/rust-1.96%2B-orange.svg" alt="MSRV 1.96" />
</p>

**pohunek** is a single-user control plane for durable coding-agent sessions
across your own machines. A Rust daemon (`pohunekd`) owns the PTYs and agent
processes on each host; the CLI (`pohunek`) drives it locally over a Unix
socket and remotely over a NetBird/WireGuard mesh.

**The GUI is optional.** The daemon and its protocol are the product; every
client — the CLI, the bundled desktop GUI (`pohunek-gui`), your own launcher —
sits on top of the same versioned protocol. `pohunek-gui` ships as a reference
client, not a requirement: pohunek is fully usable from the CLI alone, and the
Rust/TypeScript SDKs exist precisely so you can **build your own GUI or client**
tailored to how you work. See [SDKs and building your own client](#sdks-and-building-your-own-client).

Start Codex or Claude Code on any of your machines, detach, walk away, and
come back later — from any terminal, from a GUI, or from a keyboard
launcher. The agents keep working; pohunek keeps track of what they are doing,
where they are doing it, and when they need you.

> A *pohunek* is the farmhand boy who drives the draft animals. He does not
> plow himself — he keeps the team moving.

> **Status: pre-1.0, experimental.** Wire shapes, config files, and on-disk
> metadata may change freely between releases. Linux-first.

## Features

**Durable agent sessions**

- The daemon owns every PTY, so sessions survive client detach and terminal
  crashes — attach from any terminal with `pohunek attach`, detach with
  `Ctrl-]`, reattach later. Multiple clients can attach to one session.
- Codex and Claude Code are first-class agents (plus plain `shell`), with
  per-host **agent profiles** that define the program, arguments, environment,
  and input rules for custom runtimes (e.g. `claude-otel`).
- **Live agent state detection** — `working` / `blocked` / `idle` — derived
  from OSC terminal titles, screen-content pattern matching, and PTY activity.
  Detection rules are TOML manifests, so new agents can be added without
  recompiling.
- **Native resume**: hooks capture the agent's own session id, so a stopped
  session resumes the original conversation instead of replaying commands.
- **Session fork** — branch a Claude Code conversation into a new session and
  PTY without disturbing the original.
- **Prompt injection done right**: `session input` and `--input` use per-agent
  framing (bracketed paste, delayed submit) so multi-line prompts actually
  submit into Ink/TUI agents instead of being half-swallowed.
- Rename sessions, attach arbitrary `key=value` metadata, and inspect
  everything as JSON.

**Multi-host, no central server**

- Every command takes `--host <name>`; session targets accept
  `<host>/<session-id>`. The CLI talks **directly** to each host's daemon —
  there is no coordinator, no SaaS, no state sync.
- Remote transport is a TCP listener bound **only** to the host's
  NetBird/WireGuard address, never `0.0.0.0`. Reachability and encryption come
  from the mesh; local access is an owner-only Unix socket.
- **Tokenless discovery**: `pohunek host discover` enumerates NetBird peers
  and probes which of them run a reachable daemon; `host inspect` queries live
  capabilities (supported agents, worktree support) straight from the daemon.

**Projects and worktree isolation**

- The daemon notices when a session starts inside a git repository and records
  a lightweight **project** (keyed by the canonical git common dir, so a repo
  and all its worktrees collapse into one project). No filesystem scanning.
- Start a session with `--branch` and the daemon creates a
  **worktree-per-session** off the base branch, so two agents never share a
  working tree by accident. Worktree ownership is recorded and checked before
  any reuse or cleanup.
- Per-project **actions and prompt templates**: an in-repo `.pohunek/`
  directory shadows host-level config, so `pohunek project action <ref> <name>`
  resolves a full launch recipe (agent, base branch, branch rule, rendered
  prompt) for launchers, the GUI, and scripts.
- `pohunek session diff` renders a unified diff of a session's worktree
  against its base — including untracked files — over the wire.

**Durable notifications inbox**

- Agent events (approval required, agent blocked, turn completed, session
  finished, errors) become **durable notification records** with lifecycle
  states (`unread → read → acknowledged → archived → deleted`).
- Fed by installed Codex/Claude hooks *and* daemon-side state projection, with
  source-priority dedupe, a debounce window that drops notifications the agent
  resolves itself, and resolve-on-resume so stale "blocked" entries disappear
  when the agent starts working again.
- `pohunek notifications list|watch --all-hosts` fans out across every
  reachable host client-side; per-kind and per-provider policy plus retention
  pruning are daemon-enforced.

**Native desktop GUI (optional reference client)**

- `pohunek-gui` (Iced, Wayland) is a keyboard-first control plane: hosts,
  sessions, projects, worktrees, and live agent activity in one window. It
  deliberately embeds **no terminal** — opening a session spawns your own
  terminal via a configurable `attach_command`.
- **Agents monitor** with working/blocked/idle counts, `b` cycles through
  blocked agents; the **Inbox** modal is a cross-host triage view over durable
  notifications and raises OS notifications only for action-required and
  error records.
- **Linear and GitHub integration** (client-side: Linear GraphQL via keyring
  token, GitHub via `gh`): browse issues and PRs with named filters, then
  launch a linked agent session from a work item — the session carries
  `link.*` metadata tying it back to the issue/PR.
- **Review tab**: browse a session's worktree diff (or a PR diff), leave
  inline comments, and dispatch the review as a *new agent session* running in
  the same worktree, prompt rendered from a template.

**Launcher and terminal UX**

- `pohunek setup` installs rofi/sway launcher scripts, default config, prompt
  templates, and an optional sway keybinding drop-in — start or switch to any
  session in two keystrokes.
- Optional **attach banner**: the attach client parses the agent's byte stream
  into its own screen model and composites a one-row status banner that works
  even under full-screen TUIs; `Ctrl-\` opens a session menu (kill, detach,
  new session in the same worktree, fork, rename).
- Attach auto-reconnects after a daemon restart when the session is resumable.

**Built to be driven by agents, not just humans**

- Every command has `--json`; errors are structured (`class`/`code`/`msg` plus
  a recovery hint); `subscribe` streams typed events (session lifecycle, agent
  state, notifications) over the same protocol.
- **Universal assistant**: `pohunek assistant "how do I …"` launches a capable
  agent session preloaded with an offline knowledge bundle about pohunek
  itself and a redacted live snapshot of your hosts — self-hosted support for
  setup, project configuration, updates, and debugging.
- **SDKs — build your own GUI or client**: a Rust client crate
  (`pohunek-client`) and TypeScript packages (`@pohunek/protocol`,
  `@pohunek/sdk`, `@pohunek/backend`, `@pohunek/client-core`,
  `@pohunek/frontend`, and `@pohunek/testkit`) speak the same versioned
  newline-delimited JSON protocol the bundled GUI uses — nothing is private to
  `pohunek-gui`. Browsers use the node-free `@pohunek/sdk/browser` entry through
  the backend's WebSocket tunnels. TS protocol types are generated from the Rust
  source of truth. If the bundled clients do not fit your workflow, wire up your
  own control plane on these SDKs instead of forking one.

## How it works

```text
  CLI / GUI (local)                   CLI / GUI (remote)
       |                                   |
       | Unix socket                       | TCP over NetBird/WireGuard
       | ($XDG_RUNTIME_DIR, mode 0600)     | (daemon binds ONLY to the 100.x iface)
       v                                   v
 +-----------------------------------------------------------+
 |                    host daemon (pohunekd)                  |
 |                                                            |
 |  control protocol: newline-delimited JSON                  |
 |  attach stream:    separate raw byte connection per PTY    |
 |                                                            |
 |  PTYs + agents | metadata | events + notifications | mesh  |
 +-----------------------------------------------------------+
       |
   Codex / Claude Code running in daemon-owned PTYs
```

Each host is authoritative for its own sessions, projects, worktrees, and
notifications. Control traffic is newline-delimited JSON; attaching to a
session opens a **separate raw byte connection**, so JSON stays JSON and
terminal bytes stay bytes.

Durability is tiered and honest: detach and client restarts are free; a
**daemon restart kills live PTYs** by design, but session metadata, worktrees,
and native agent conversations survive and can be resumed.

## Install

Each release publishes per-component archives for x86_64 Linux (glibc and
MUSL): `pohunek-cli-*`, `pohunek-daemon-*`, and `pohunek-gui-*`. Every archive
contains the binary, license, and the offline documentation bundle under
`docs/offline/`.

Download from [Releases](https://github.com/zajca/pohunek/releases), unpack,
and put the binaries on your `PATH`.

Or build from source (Rust 1.96+):

```bash
git clone https://github.com/zajca/pohunek.git
cd pohunek
cargo build --release --locked --bin pohunek --bin pohunekd --bin pohunek-gui
```

## Quick start

```bash
# 1. Check the environment (binaries, socket paths, writable state dirs)
pohunek doctor

# 2. Start the host daemon in the background
pohunek daemon start --detach
pohunek health

# 3. Install agent hooks (native session-id capture + notifications)
pohunek integration install

# 4. Start an agent session and attach to it
pohunek session new --agent claude --name "fix-login-bug"
pohunek session list
pohunek attach <session-id>        # Ctrl-] detaches, the agent keeps running
```

For an isolated feature branch, let the daemon create a dedicated worktree:

```bash
pohunek session new --agent codex \
  --repo ~/Code/myapp --branch feat/retry-logic --base-branch main \
  --input "Add retry logic to the API client, then run the tests."
```

## CLI guide

Every command accepts `--host <name>` (default `local`), and nearly all of
them `--json` for machine-readable output (the exceptions are `attach`,
`daemon start`, and `prompt render`). Session targets are `<session-id>` or
`<host>/<session-id>`.

| Command | What it does |
|---|---|
| `pohunek doctor` | Environment health: binaries, socket, state dirs, NetBird, agents. |
| `pohunek daemon start [--detach]` | Run the host daemon (foreground or background). |
| `pohunek health` / `status` | Daemon liveness, build, and protocol version. |
| `pohunek session new` | Start a session: `--agent`, `--name`, `--project`/`--repo`, `--branch`, `--base-branch`, `--cwd`, `--input`, `--meta k=v`. |
| `pohunek session list` | List sessions; `--filter state=running --filter agent=codex` (ANDed), `-q` for ids only. |
| `pohunek session inspect <target>` | Full session record: state, activity, cwd, project, branch, worktree, resume binding. |
| `pohunek attach <target>` | Attach the current terminal; `Ctrl-]` detaches. |
| `pohunek session input <target> <text>` | Inject a prompt with agent-correct framing. |
| `pohunek session fork <target>` | Fork an agent conversation into a new session (Claude Code). |
| `pohunek session diff <target> [--base <ref>]` | Unified diff of the session's worktree vs its base. |
| `pohunek session rename / stop / rm` | Rename, stop, or evict a session. |
| `pohunek project add / list / show / rename / rm` | Manage git-repo-aware project records. |
| `pohunek project actions / action / prompt` | Resolve per-project launch recipes and prompt templates. |
| `pohunek host discover / list / inspect` | Find NetBird peers running daemons and query live capabilities. |
| `pohunek notifications list / watch` | Inspect or stream the durable inbox; `--all-hosts` fans out. |
| `pohunek notifications read / ack / archive / delete` | Drive one record's lifecycle (`host/id` targets a specific host). |
| `pohunek notifications policy / retention` | Per-kind/provider policy, retention pruning (`--dry-run` / `--apply`). |
| `pohunek integration install` | Install Codex/Claude hooks for resume capture and notifications. |
| `pohunek setup [scripts\|config\|sway]` | Install launcher scripts, default config + prompt templates, sway keybindings. |
| `pohunek assistant [intent] [request…]` | Launch the self-help assistant with knowledge bundle + live snapshot. |
| `pohunek prompt render / link` | Render provider prompt templates and work-item link metadata (used by launchers). |

### Working across hosts

```bash
pohunek host discover                          # which NetBird peers run a daemon?
pohunek host inspect buildbox --json           # agents/worktree capabilities, live

pohunek session new --host buildbox --project myapp --agent codex \
  --branch feat/parser --input "Fix the parser fuzz failures."

pohunek session list --host buildbox
pohunek attach buildbox/s-3                    # raw PTY over the mesh

pohunek notifications watch --all-hosts        # one triage stream for every machine
```

Remote session starts ask for confirmation (skip with `--yes`); project
references resolve on the *target* host, so no filesystem path ever crosses
the wire.

### Notifications triage

```bash
pohunek notifications list --unread
pohunek notifications ack buildbox/n-42
pohunek notifications policy set --provider claude --kind turn_completed --enabled
pohunek notifications retention prune --status archived --before 2026-06-01T00:00:00Z --apply
```

### Assistant

```bash
pohunek assistant "why does attach fail on my laptop?"
pohunek assistant setup                # steer toward host setup
pohunek assistant debug --host buildbox --no-snapshot
```

The assistant is an ordinary agent session — the same PTY, attach, and
notification machinery — launched with a materialized offline knowledge
bundle and a redacted snapshot of live state. No secrets enter the prompt.

## GUI

`pohunek-gui` reads `~/.config/pohunek/gui.toml`:

```toml
pohunek_bin = "/usr/local/bin/pohunek"
attach_command = "$TERMINAL -e sh -c 'exec {bin} attach --host {host} {id}'"

[providers.linear]
token_key = "linear-token-ref"   # keyring entry name — never a token value
endpoint = "https://api.linear.app/graphql"
token_timeout_ms = 5000          # required: caps keyring token lookup

[providers.github]
gh_bin = "gh"
```

Highlights: persistent right-pane tabs (`1 Detail · 2 Linear · 3 GitHub ·
4 Worktrees · 5 Review`), `n` opens the Start-session modal, `a` the
assistant modal, `i` the Inbox, `b` cycles blocked agents, `/` searches
provider lists,
`shift+?` shows the full keymap. All bindings are remappable via a
`[keybindings]` table. Wayland-only on Linux v1.

The bundled GUI is a **reference client**, not the only supported way in. It
uses the same public protocol and SDKs documented below — so if it does not fit
your workflow, the next section is your starting point for building your own.

## Web control center

The optional web control center serves one Svelte SPA for host and session
status, session lifecycle, live notifications, and in-browser terminal attach.
`@pohunek/backend` discovers daemons through its local `pohunekd` and exposes
the existing protocol as transparent WebSocket tunnels; it holds no
authoritative session state, and the CLI and native GUI remain independent.

The workspace is a persistent session-first shell. Its rail groups sessions by
project, promotes blocked work into an Attention section, searches and filters
across every host, and keeps compact host connectivity visible without making
hosts the primary navigation. Selecting a running session attaches its terminal
in the main pane while the rail remains available. Session metadata and stop
actions live in an inspector drawer; new-session and Inbox flows are overlays,
and opening a session notification marks it read and selects its terminal.

Keyboard controls are available outside form fields and terminals: `Ctrl+K`
opens the command palette, `Ctrl+B` toggles the session rail, `n` starts a
session, `i` opens the Inbox, `b` cycles blocked sessions, `/` focuses session
search, and `j`/`k` or the arrow keys move focus through the rail before
`Enter` opens the focused session. `Esc` closes the active overlay. Unmodified
shortcuts never intercept input inside the embedded terminal.

On mobile and short touch viewports, the session rail becomes an off-canvas
drawer so the terminal owns the screen. The terminal follows the visual
viewport when the software keyboard opens and provides a touch toolbar for
keyboard focus, Escape, Tab, one-shot Control and Alt modifiers, Control-C,
and the arrow keys. Mobile overlays use the full viewport, controls provide
44-pixel touch targets, and safe-area insets are respected in portrait and
landscape orientations.

For local UI development with two fixture daemons, run `bun run dev` from
`web/`; no Rust daemon or NetBird setup is required. Bun remains the workspace
runtime, while the development orchestrator requires `node` on `PATH` to run
Vite's WebSocket proxy in a compatible Node child process. Set
`POHUNEK_NODE_BIN` only when the Node executable has a nonstandard path. A
deployed backend binds only to a NetBird address (loopback requires the explicit
development flag; wildcard binds are rejected). The supplied systemd user unit
and its environment file instructions are in
`web/backend/systemd/pohunek-backend.service`.

## SDKs and building your own client

pohunek's real interface is its **protocol**, not any one client. `pohunek-gui`
is just one consumer of a versioned, newline-delimited JSON protocol that every
client speaks — and the same protocol and SDKs are available to you. You are
encouraged to **build your own GUI, TUI, launcher, or automation** on top of
them rather than being tied to the bundled app.

Nothing the GUI does is private to the GUI: it drives hosts, sessions,
projects, worktrees, notifications, diffs, and `subscribe` event streams
entirely through this surface.

- **Rust** — the `pohunek-client` crate: a typed `Client`, transports for local
  Unix sockets and NetBird/WireGuard TCP, raw attach helpers, typed
  `ClientError`s, and `subscribe` streams. It re-exports the `protocol` crate,
  the source of truth for every request, response, and event type.
- **TypeScript** — `@pohunek/protocol` (types generated from the Rust protocol),
  `@pohunek/sdk` (Bun/Node plus shared runtime), its browser-safe
  `@pohunek/sdk/browser` entry, `@pohunek/backend` (host discovery, static SPA,
  and transparent WebSocket tunnels), `@pohunek/client-core` (headless
  multi-host state), `@pohunek/frontend` (the Svelte SPA), and
  `@pohunek/testkit` (the stateful fixture daemon used by tests and dev mode).
- **Contract** — the wire surface is documented in
  [`docs/public-api.md`](docs/public-api.md); TS types are regenerated from
  Rust so the two SDKs never drift. The protocol is versioned, but pre-1.0 it
  may still change between releases (see the status note above).
- **Or skip a client entirely** — every CLI command supports `--json` and
  `subscribe` streams typed events, so a shell script is a legitimate way to
  drive pohunek.

Connecting and listing sessions is the same call in both SDKs:

```rust
// Rust — `pohunek-client`
use pohunek_client::{protocol::method::SessionList, Client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sock = format!("{}/pohunek/daemon.sock", std::env::var("XDG_RUNTIME_DIR")?);
    // Local daemon over its owner-only Unix socket.
    let mut client = Client::connect_local(&sock).await?;
    // ...or a remote host over the NetBird/WireGuard mesh:
    // let mut client = Client::connect("workstation", &sock).await?;

    let sessions = client.call::<SessionList>(Default::default()).await?;
    for s in sessions {
        println!("{} {:?} {:?}", s.id.0, s.state, s.activity);
    }
    Ok(())
}
```

```ts
// TypeScript — `@pohunek/sdk`
import { connectLocal } from "@pohunek/sdk";

const sock = `${process.env.XDG_RUNTIME_DIR}/pohunek/daemon.sock`;
const client = await connectLocal(sock);

const sessions = await client.call("session.list", {});
for (const s of sessions) {
  console.log(s.id, s.state, s.activity);
}
await client.close();
```

Browsers use the node-free entry and reach a daemon through the backend origin:

```ts
import { Client } from "@pohunek/sdk/browser";

const client = await Client.connectWs(window.location.origin, "workstation");
```

Or subscribe to the same live event stream the CLI and GUI consume — session
lifecycle, agent state, and notifications, decoded into typed events:

```rust
// Rust — subscribe consumes the client and hands back an event stream.
use pohunek_client::{next_request_id, protocol::{method, Request}, Client};
use serde_json::Value;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sock = format!("{}/pohunek/daemon.sock", std::env::var("XDG_RUNTIME_DIR")?);
    let client = Client::connect_local(&sock).await?;

    let request = Request::new(next_request_id(method::SUBSCRIBE), method::SUBSCRIBE, Value::Null);
    let mut events = client.subscribe(&request).await?;
    // Runs until the daemon closes the stream.
    while let Some(ev) = events.next_event().await? {
        // `event` is the name (e.g. "agent_state"); `payload` is the flattened JSON body.
        println!("{}: {}", ev.event, ev.payload);
    }
    Ok(())
}
```

```ts
// TypeScript — same event stream.
import { connectLocal, nextRequestId } from "@pohunek/sdk";
import { PROTOCOL_VERSION } from "@pohunek/protocol";

const client = await connectLocal(sock);
const subscription = await client.subscribe({
  v: PROTOCOL_VERSION,
  id: nextRequestId("subscribe"),
  method: "subscribe",
  params: null,
});

for (let ev = await subscription.nextEvent(); ev !== null; ev = await subscription.nextEvent()) {
  console.log(ev.event, ev);
}
```

## Trust boundary

pohunek is built for **one operator on machines they own**:

- Local access control is owner-only socket and file permissions.
- Remote access control is your NetBird/WireGuard network and its policies;
  the daemon never binds a public interface.
- There is no multi-user auth, no hosted control plane, and no tenant model —
  by design, not omission.
- Secrets stay out of structured state: metadata, events, notifications,
  prompts, and logs are secret-free; provider tokens live in the OS keyring or
  provider CLIs (`gh`). Raw terminal scrollback is the one honest exception —
  it is stored owner-private.

## Development

The workspace is a Cargo monorepo (edition 2021, MSRV 1.96) plus a Bun
workspace in `web/` for the TypeScript packages.

| Crate | Role |
|-------|------|
| `crates/protocol` | Wire contract: envelopes, methods, events, version negotiation. |
| `crates/client` | Rust SDK: typed errors, transports, attach helpers. |
| `crates/daemon` | `pohunekd`: PTY ownership, session registry, detection, notifications. |
| `crates/cli` | `pohunek`: every command over the control protocol. |
| `crates/gui-core` | Headless GUI state + SDK bridge (no Iced; fully unit-testable). |
| `crates/gui` | Native Iced shell wrapping `gui-core`. |
| `crates/prompt` | Shared prompt rendering + `link.*` metadata schema (CLI, GUI, scripts). |
| `crates/knowledge` | Knowledge-bundle primitives for the assistant and offline docs. |
| `crates/terminal` | VT screen tracking and attach compositing. |
| `crates/netbird` | NetBird status parsing, host resolution, bind validation. |
| `crates/paths` / `crates/hostcheck` | XDG/socket contract; host environment probes. |
| `crates/xtask` | Workspace automation: docs build/check, TS type generation. |
| `web/` | `@pohunek/protocol`, `@pohunek/sdk`, `@pohunek/backend`, `@pohunek/client-core`, `@pohunek/frontend`, `@pohunek/testkit`. |

Read **[AGENTS.md](AGENTS.md)** first — it is the canonical contributor guide.
Authoritative design lives in [docs/architecture.md](docs/architecture.md);
the protocol contract in [docs/public-api.md](docs/public-api.md).

### Gates

CI treats warnings as errors. Run the full set before calling anything done:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
cargo build --workspace --release
cargo xtask docs check          # knowledge bundle: schema/drift/secrets/runbooks
```

Web workspace:

```bash
cd web
bun install --frozen-lockfile
bun run typecheck && bun run lint && bun test
```

A protocol change is not done until the generated TypeScript types match:

```bash
cargo xtask ts generate   # regenerate web/shared/src/generated/**
cargo xtask ts check      # CI gate
```

### Conventions that matter here

- **Rust guidelines are mandatory.** The Microsoft Pragmatic Rust Guidelines
  are vendored at `.agents/rust-guidelines/`; read the relevant files before
  touching any `.rs` file (`SKILL.md` is the index).
- **Typed errors** (`thiserror`) per crate; no bare catch-alls. Library crates
  `#![forbid(unsafe_code)]`.
- **Config fails fast** — required values are validated at load; no silent
  defaults. No hardcoded magic values.
- **Secrets never enter code, logs, errors, or agent context.** Keyring
  references only; `gh` output is redacted before it can reach an error.
- **Protocol ripples**: touching `crates/protocol` means updating `client`,
  `daemon`, `cli`, `gui-core`, the generated TS types, `docs/public-api.md`,
  and the `docs/knowledge/` bundle in the same change.
- **Tests for all new logic**; the protocol and state machines have rich
  suites — extend them.

### Release

`scripts/release` bumps the workspace version, tags `vX.Y.Z`, and pushes; the
Release workflow re-runs the gates on the tag, then builds and publishes the
glibc and MUSL x86_64 component archives with the offline docs bundled in.

## License

[MIT](LICENSE)
