# pohunek Idea

> **Status / Revised Direction (current).** This file is the original, broad
> brainstorm and is kept for context. The **committed direction** is narrower and
> lives in [`docs/architecture.md`](docs/architecture.md) and the phase docs.
> Where they disagree, the architecture doc wins. Key changes from this
> brainstorm: single-user personal tool (no multi-user authz, no SSH bridge, no
> signed-manifest mesh, no key rotation); remote transport is **direct over
> NetBird/WireGuard**; discovery is **tokenless NetBird-local + live capability
> query**; agent state comes from OSC terminal titles + screen-content matching
> + PTY activity (hooks capture only the session ID for resume); providers
> (Linear/GitHub) are **deferred and shell-out based**; the
> libghostty GUI is **deferred** (still the eventual target). Codex and Claude
> Code remain first-class agents.

## Working Name

pohunek. The product name and prototype CLI binary name are both `pohunek`.

## One-Line Vision

A CLI-first, SSH/VPN-native agent workspace that lets developers discover trusted hosts, run durable agent sessions anywhere, attach through a libghostty-powered terminal client, and connect work items, repositories, PRs, and review workflows without relying on a central control server.

## Why This Should Exist

Modern coding-agent workflows are moving from one local terminal to many concurrent, long-running agents spread across local machines, workstations, servers, Docker hosts, and private development networks. Existing tools solve pieces of this problem, but the ideal system should combine:

- durable PTY ownership from a background service;
- terminal-native multiplexing and accurate agent state;
- remote execution over ordinary SSH;
- first-class task and code-host integrations;
- a fast terminal UI built on libghostty;
- a great CLI that both humans and agents can operate;
- peer discovery across private VPN networks without a central application server.

The core product should feel like a distributed terminal control plane for agent work, not a hosted SaaS, Electron dashboard, or one-off tmux wrapper.

## Inspirations

### zremote

What to keep:

- A background agent service owns the PTY and process lifecycle.
- Clients are thin views/controllers that can connect, disconnect, and reconnect.
- Terminal sessions are not tied to a single foreground client process.

How to adapt it:

- Replace central-server coordination with direct SSH/VPN connectivity.
- Keep durable PTY ownership per host, but make host-to-host control peer-to-peer.
- Treat every host as both a session owner and a possible client.

### herdr

What to keep:

- Terminal-native agent multiplexer with workspaces, tabs, panes, detach, and reattach.
- Agent states such as blocked, working, done, and idle shown at a glance.
- Real terminal panes rather than interpreted or re-rendered agent transcripts.
- Session restore using native agent session identity where available.
- SSH-friendly operation and a socket/CLI API that agents can use.

How to adapt it:

- Make remote hosts first-class objects, not just remote attach targets.
- Add cross-host discovery, inventory, and capability exchange.
- Keep the mental model terminal-native while adding richer task/code integrations.

### Kandev

What to keep:

- Multi-agent support through ACP where possible and PTY/TUI fallback where needed.
- Work item integration, especially Linear first, with an adapter model for Jira, GitHub Issues, GitHub Projects, and other trackers.
- Code-host integration, especially GitHub first, with an adapter model for Forgejo, GitLab, and other providers.
- Review-first workflows around worktrees, diffs, PRs, and human approval gates.
- Flexible executors: local process, Docker, SSH, and later cloud/Kubernetes.

How to adapt it:

- Make CLI and terminal workflows primary instead of a web dashboard.
- Keep project/task orchestration, but avoid requiring a central backend.
- Store and sync only the metadata needed to coordinate work across trusted peers.

### cmux

What to keep:

- libghostty as the terminal rendering foundation for a fast, native-feeling terminal surface.
- Vertical workspace/session navigation with visible metadata.
- Notification model for agents that need attention.
- Scriptable CLI/socket controls for panes, splits, notifications, and browser/dev-server workflows.
- Safe session restore behavior that avoids blindly replaying unsafe commands.

How to adapt it:

- Keep libghostty in the interactive client, while the daemon owns PTYs through the OS.
- Make remote sessions and agent state the central use case, not just local panes.
- Prefer composable primitives over an opinionated workflow that locks users in.

## Product Principles

- CLI first: every meaningful action must be possible through a polished CLI.
- Agent-operable: an agent should be able to inspect, plan, start sessions, attach helpers, read state, and hand work back to the user through documented commands or a local API.
- No central app server: coordination happens through trusted peers over SSH/VPN.
- Durable by default: agent sessions must survive client disconnects and routine restarts where the underlying agent supports resume.
- Real terminals: keep actual PTY sessions and agent TUIs available.
- Adapter-based integrations: Linear and GitHub are first-class, but not hardcoded as the only possible providers.
- Human approval gates: the system helps agents work in parallel, but humans remain responsible for review and shipping.
- Secure private-network posture: trust comes from SSH identities, VPN identities, explicit policy, and local secrets management.

## Resolved Early Decisions

- Product and CLI name: `pohunek`.
- First implementation stack: Rust daemon and Rust CLI first; GUI comes later.
- GUI platform target: the GUI is Linux-first. macOS and Windows compatibility can be explored later, but they should not shape the first GUI architecture or block the Linux client.
- libghostty strategy: build a small Linux-first mini GUI proof of concept early to validate integration risk and basic UX, but do not block the daemon/CLI MVP on a full GUI client.
- Local daemon API: use the Herdr-style approach, with a local socket and newline-delimited framed JSON requests, responses, and subscription events.
- Agent protocol: start with PTY/TUI agents only; defer ACP JSON-RPC 2.0 over stdin/stdout until the daemon, PTY, state, and resume model are stable.
- Fallback agent mode: use real PTY/TUI sessions for agents that do not support ACP or whose native terminal UI is still the best control surface.
- Public/web transport: avoid making WebSocket the core daemon protocol; reserve WebSocket or a dedicated streaming protocol for later GUI/client layers when needed.
- Mesh metadata model: use signed snapshots/manifests for cross-host sync, while each host keeps a local append-only event log for audit, debugging, and future replay.
- VPN discovery model: tokenless/local-first discovery is the default; VPN provider cloud APIs are optional enrichments for stronger metadata, policy, and team inventory.
- First reference agents: support both Codex and Claude Code early through PTY/TUI mode, using them to validate the adapter boundary, state detection, and resume behavior before adding ACP.

## Core User Experience

### Human CLI

The CLI should be the primary product surface, not a thin admin tool.

Example commands:

```bash
pohunek doctor
pohunek init
pohunek host list
pohunek host discover --vpn tailscale
pohunek host discover --vpn netbird
pohunek host join ssh://workbox
pohunek host inspect workbox
pohunek session new --host workbox --repo github.com/acme/api --work-item LIN-123
pohunek agent run codex --host workbox --instructions plan.md
pohunek attach workbox/session-42
pohunek status
pohunek inbox
pohunek task list --provider linear
pohunek pr open --provider github --session session-42
pohunek review session-42
```

CLI requirements:

- consistent command grammar;
- fast startup;
- useful default table output;
- `--json` output for automation and agents;
- streaming output for long-running operations;
- shell completions;
- local and remote targets using the same command shapes;
- clear errors with suggested recovery commands;
- no hidden cloud dependency.

### Agent-Operated Mode

The system should include an agent-facing control surface, exposed through the CLI and a local socket API.

An agent should be able to:

- list trusted hosts and capabilities;
- find available repositories and worktrees;
- claim or create work items;
- start helper agents on another host;
- split panes or create sessions for subtasks;
- read summarized state and recent terminal output;
- wait for state changes such as blocked, done, or failed;
- open PRs or attach work to existing PRs through provider adapters;
- ask the user for approval through the notification/inbox system.

The user should be able to start an "operator agent" with instructions that explain how to control the whole mesh. From there, the user can work mostly inside the agent conversation while the agent uses the system primitives.

### Terminal Client

The interactive client should use libghostty for terminal rendering and should target Linux first. The first production GUI should assume Linux desktop constraints, packaging, display server behavior, keyboard handling, and font/rendering integration. Other platforms are later compatibility targets, not first-version requirements.

It should provide:

- workspaces grouped by host, repository, task, and branch;
- panes and splits for real PTY sessions;
- vertical navigation with state badges;
- agent attention indicators;
- notification inbox;
- attach/detach;
- safe restore;
- optional browser/dev-server pane later.

The daemon should not depend on libghostty for PTY ownership. The daemon owns OS PTYs and processes; libghostty belongs in the client rendering layer.

## Recommended Architecture

### Option A: Peer Mesh Over SSH/VPN

Each host runs a local daemon. The daemon owns PTYs, agent processes, local metadata, worktrees, and host capabilities. CLIs and clients connect locally through a Unix socket. Remote control uses SSH to run a small bridge command on the target host, or to connect to the target daemon through an authenticated local channel.

The first daemon protocol should be newline-delimited JSON over a local socket. A client sends one framed JSON request per line; the daemon replies with one JSON response line for normal requests. Long-lived subscriptions keep the same connection open and stream event envelopes as newline-delimited JSON. This keeps the protocol simple to debug, easy for agents to operate, and suitable for SSH bridging.

Discovery uses provider adapters:

- Tailscale: tailnet device inventory, MagicDNS names, tags, and Tailscale SSH where enabled.
- NetBird: peer inventory, DNS/names, ACL-aware private addresses, and standard OpenSSH over the NetBird network unless a stronger provider-native SSH identity layer is available.
- Generic SSH: `~/.ssh/config`, static inventory files, and explicit `pohunek host join ssh://...`.

Discovery must work without cloud API tokens for the normal single-user path. The first layer should inspect local VPN state, local DNS names, SSH config, known hosts, and explicitly joined hosts. Provider cloud APIs can be configured later to enrich metadata, validate policy, or import team inventory, but they should not be required to discover and trust reachable hosts inside a private network.

Hosts exchange signed manifests containing host identity, capabilities, daemon version, supported agents, available runtimes, and non-secret configuration references. The mesh stores eventual-consistency metadata, while each host remains authoritative for its own live sessions.

This is the recommended direction because it satisfies the main constraint: multi-host operation without a central application server.

Metadata should use a hybrid model. Cross-host sync should exchange signed current-state snapshots and manifests, because they are easy to inspect, reconcile, and replace when hosts reconnect. Each host should also keep a local append-only event log for session lifecycle, trust decisions, command approvals, provider actions, and notable errors. The local log gives auditability without forcing the first release to solve distributed log replication.

### Option B: Local-First With Optional Coordinator

The core still works locally and over SSH, but users may run an optional coordinator for inventory, search, and team policy. This simplifies discovery and sync, but it risks becoming the same central-server model the project is trying to avoid.

This can be a later enterprise/team feature, not the default architecture.

### Option C: Pure SSH Without Daemons

Every command shells into a host and runs the agent directly. This is simpler to bootstrap but loses durable PTY ownership, rich state, fast attach, reliable resume, and agent-to-agent orchestration.

This should be rejected for the core product. It can exist only as a degraded fallback mode.

## System Components

### Host Daemon

Responsibilities:

- own PTYs and agent processes;
- track sessions, panes, workspaces, worktrees, and runtime status;
- expose a local Unix socket API;
- emit structured events;
- store local metadata in SQLite or another embedded store;
- manage safe resume bindings;
- supervise local, Docker, and SSH executor processes;
- report host capabilities to trusted peers.

Implementation direction:

- Rust first.
- Local socket transport first, using newline-delimited JSON frames.
- Typed request/response/event schema in Rust, serialized with Serde.
- WebSocket is not part of the core daemon API in the first phase.

### CLI

Responsibilities:

- provide the main human workflow;
- provide stable machine-readable commands for agents;
- connect to local daemon or remote daemon through SSH;
- bootstrap hosts;
- manage discovery, trust, and configuration;
- run diagnostics.

### libghostty Client

Responsibilities:

- render interactive terminals;
- manage tabs, panes, splits, and workspace navigation;
- show host/session/agent state;
- surface notifications and pending approvals;
- attach to local or remote daemon streams.

### Mesh Discovery Layer

Responsibilities:

- discover hosts through VPN adapters and static SSH inventory;
- verify host identity;
- exchange signed host manifests;
- maintain a local peer cache;
- avoid opening extra public ports;
- degrade gracefully when VPN provider metadata is unavailable.

Provider API posture:

- default discovery must not require Tailscale, NetBird, or other cloud API tokens;
- local provider CLIs and OS/network state are preferred where available;
- cloud API tokens are opt-in and only unlock richer inventory, tags, ACL context, and team policy checks;
- absence of provider API access should never block explicit `pohunek host join ssh://...`.

### Agent Runtime Layer

Responsibilities:

- support TUI agents in PTYs first;
- defer ACP until the daemon, PTY, state, and resume model are stable;
- detect state through native integration, process state, terminal output heuristics, and explicit socket reports, with ACP state added later when ACP support lands;
- store native session IDs when supported;
- resume sessions after client restart and, when possible, daemon restart.

The first version should treat real PTY/TUI control as the primary runtime path. This keeps Codex and Claude Code behavior visible and debuggable while the daemon/session model is still being proven. ACP should remain a planned adapter model, following the Kandev-style transport when it is added: JSON-RPC 2.0 over the agent subprocess stdin/stdout, normalized into the daemon event model.

First-class agents should include Codex and Claude Code from the beginning through PTY/TUI mode. They should validate the adapter boundary together because they exercise different practical requirements around terminal control, session identity, resume behavior, prompts, permissions, and observable state. Later agents should include GitHub Copilot CLI, Gemini CLI, OpenCode, Amp, Cursor Agent, and others through adapters.

### Work Item Adapter Layer

Linear should be first-class, but behind a general interface.

Common model:

- provider;
- workspace/project/team;
- work item ID;
- title;
- description;
- status;
- assignee;
- labels;
- comments;
- linked branches;
- linked commits;
- linked PRs;
- workflow transitions.

Adapters:

- Linear first;
- GitHub Issues and GitHub Projects;
- Jira;
- later GitLab Issues, Forgejo Issues, and custom local markdown/task files.

### Code Host Adapter Layer

GitHub should be first-class, but behind a general interface.

Common model:

- provider;
- organization/owner;
- repository;
- branch;
- pull request or merge request;
- review comments;
- checks;
- commits;
- releases;
- repository permissions.

Adapters:

- GitHub first;
- Forgejo;
- GitLab;
- later Bitbucket or custom Git remotes.

### Workspace Isolation

Use git worktrees for concurrent agent work. A session should have:

- host;
- repository;
- base branch;
- working branch;
- worktree path;
- linked work item;
- linked PR;
- assigned agent;
- logs and events;
- restore binding.

## State and Resume Model

Durability tiers:

1. Client detach: PTY and process continue because the host daemon owns them.
2. Client restart: layout, scrollback snapshot, host list, and session metadata restore.
3. Daemon restart: sessions restore only when native agent resume or explicit safe resume binding exists.
4. Host restart: worktrees, metadata, and resumable agent conversations remain; arbitrary live processes do not.

Resume safety rules:

- never store secrets from environment snapshots;
- store command prefixes, working directory, and minimal environment only when approved;
- require explicit approval for auto-running custom resume commands;
- prefer native agent resume IDs over replaying shell commands;
- make restore actions visible in audit logs.

## Discovery and Onboarding Flow

### First Host

```bash
pohunek init
pohunek daemon start
pohunek doctor
```

This creates local configuration, starts the daemon, creates a local trust root, and checks available agent CLIs, Git, SSH, Docker, Tailscale, NetBird, and provider credentials.

### Additional Host

```bash
pohunek host join ssh://workbox
```

The joining flow should:

- verify SSH reachability;
- install or locate the daemon binary;
- exchange host identities;
- copy non-secret mesh configuration;
- register host capabilities;
- validate VPN visibility;
- show the exact trust decision to the user.

### VPN Discovery

```bash
pohunek host discover --vpn tailscale
pohunek host discover --vpn netbird
pohunek host trust workbox
```

Discovery finds candidates, but trust is explicit. A discovered host should not receive commands until trusted.

### Mesh Propagation

Once trusted, hosts exchange signed manifests so each host can learn about the others. Live session streams remain direct to the owning host. The mesh syncs metadata, not bulk terminal streams.

## Security Model

Trust boundaries:

- VPN membership proves network reachability, not full application trust.
- SSH identity proves connection identity, but the user still approves host enrollment.
- Each host is authoritative for its own sessions and local secrets.
- Provider credentials remain local unless the user explicitly configures shared secret storage.

Required controls:

- signed host manifests;
- explicit host trust;
- least-privilege provider tokens;
- no central secret replication by default;
- structured audit logs;
- command approval gates for dangerous operations;
- optional policy files for allowed agents, hosts, repositories, and providers;
- support for Tailscale ACLs/tags and equivalent VPN policy concepts;
- no automatic execution from untrusted discovery data.

Audit posture:

- local append-only event log per host;
- signed host manifests for mesh-visible state;
- event log records for trust changes, session lifecycle, command approvals, provider mutations, restore actions, and remote execution attempts;
- no first-version requirement to replicate full event logs across the mesh.

## Configuration

Configuration should live in a dedicated config directory, likely:

```text
~/.config/pohunek/
  config.toml
  hosts.toml
  providers/
  policies/
  agents/
```

Local runtime state should live separately, likely:

```text
~/.local/share/pohunek/
  state.db
  events/
  sessions/
  worktrees/
```

Logs should be structured and written to:

```text
~/.local/state/pohunek/logs/
```

Secrets should be stored in the OS keychain, provider CLIs, SSH agent, or explicit environment files that are never committed.

## MVP Scope

### MVP 0: libghostty Mini GUI Spike

- Build a minimal Linux GUI window that embeds or links libghostty.
- Render one local PTY-backed terminal session.
- Include a small session list/sidebar with static or fake session metadata.
- Support basic terminal input, resize, focus, and close behavior.
- Validate Linux build, packaging, licensing, display-server behavior, font/rendering integration, keyboard handling, and event-loop risks.
- Validate that the GUI can later attach to daemon-owned PTY streams without forcing a daemon protocol redesign.
- Keep this separate from the daemon/CLI critical path.
- Treat this as a technical and UX spike, not the first production GUI client.

### MVP 1: Durable Local Sessions

- Host daemon owns PTYs.
- CLI can start, list, attach, detach, and stop sessions.
- Basic agent state detection for both Codex and Claude Code.
- Local SQLite metadata and structured logs.
- `--json` output for all list/inspect/status commands.
- Local daemon API uses newline-delimited JSON over a local socket.
- PTY/TUI mode is the only required agent runtime in the first MVP.

### MVP 2: SSH Remote Hosts

- Add remote host enrollment through SSH.
- Run commands against remote daemons through SSH bridge.
- Start agent sessions on a chosen host.
- Attach/detach remote PTYs.
- Static inventory plus SSH config support.

### MVP 3: VPN Discovery

- Tailscale discovery adapter.
- NetBird discovery adapter.
- Explicit trust flow.
- Signed host manifests.
- Capability cache.
- Hybrid metadata model: signed mesh snapshots plus local append-only event logs.
- Tokenless discovery default with optional provider API enrichment.

### MVP 4: Work Integrations

- Linear adapter for listing, claiming, linking, commenting, and status transitions.
- GitHub adapter for repositories, branches, PR creation, PR status, and checks.
- Worktree-per-session isolation.
- Review command that summarizes changes and links task/PR/session state.

### MVP 5: libghostty Client

- Linux-first native terminal client using libghostty.
- Workspaces/tabs/panes.
- Host/session navigation.
- Agent state badges.
- Notification inbox.

## Later Scope

- Browser/dev-server pane with agent-controllable browser API.
- ACP adapter support using JSON-RPC 2.0 over stdin/stdout when the PTY-first model is stable.
- Session recording and replay.
- Policy-driven multi-agent pipelines.
- Optional team coordinator for search and policy, without becoming required.
- Kubernetes executor.
- Mobile read-only or approval client.
- Shared prompt/instruction libraries.
- Cross-provider analytics and productivity stats.
- Local web UI only if it complements, not replaces, the CLI.

## Open Questions

- No major product-direction questions are currently open. The next step is to turn this idea into a focused spec and implementation plan.

## Success Criteria

The project is on the right track when a user can:

- install the tool on three hosts inside a private VPN;
- discover the hosts without a central application server;
- explicitly trust each host;
- see host capabilities from any host;
- start a Codex or Claude Code session on a selected remote host;
- attach, detach, and reattach without killing the agent;
- see whether each agent is blocked, working, done, or idle;
- link a session to a Linear issue;
- create a GitHub branch and PR from the session;
- review diffs before shipping;
- drive the same workflow through CLI commands and machine-readable JSON;
- run an operator agent that can use the CLI/API to control the mesh;
- avoid storing provider secrets or unsafe resume commands in session snapshots.

## Reference Links

- zremote: https://github.com/zajca/zremote
- herdr: https://github.com/ogulcancelik/herdr
- Kandev: https://github.com/kdlbs/kandev
- cmux: https://github.com/manaflow-ai/cmux
- Tailscale SSH: https://tailscale.com/docs/features/tailscale-ssh
