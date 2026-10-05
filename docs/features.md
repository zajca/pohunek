# Features and architecture overview

What the pohunek core provides, how a host is put together, and the trust
boundary it is built for. The [README](../README.md) gives the short
positioning; [architecture.md](architecture.md) is the authoritative design.

## Features

**Durable agent sessions**

- A dedicated worker owns every PTY, so sessions survive client detach,
  terminal crashes, daemon restart, daemon failure, and daemon binary upgrade.
  Attach from any terminal with `pohunek attach`, detach with `Ctrl-]`, and
  reattach later. Multiple clients can attach to one session.
- Codex, Claude Code, and Hermes Agent are first-class agents (plus plain
  `shell`), with
  per-host **agent profiles** that define the program, arguments, environment,
  and input rules for custom runtimes (e.g. `claude-otel`).
- Hermes Agent `0.20.0` is supported only through its local interactive terminal
  backend. Docker, SSH, browser, desktop, gateway, ACP, and other Hermes
  backends are outside Pohunek's PTY ownership model. The selected Hermes
  profile can additionally host the owner-private Pohunek operator plugin:
  bounded typed tools, a generated skill, and best-effort lifecycle hooks.
- The Pi coding agent `1.0.x` is an optional runtime package
  (`runtime-packages/pi`) installed with `pohunek plugin install`: launch,
  detection, resume and fork need no daemon release, and the daemon assigns the
  session id Pi stores its conversation under.
- **Live agent state detection** — `working` / `blocked` / `idle` — derived
  from OSC terminal titles, screen-content pattern matching, and PTY activity.
  Detection rules are TOML manifests, so new agents can be added without
  recompiling.
- **Native recovery**: hooks capture the launch agent's own session id, so a
  lost or terminal runtime can be recovered explicitly. Recovery creates a new
  PTY generation; ordinary daemon restart reconnects to the existing worker and
  never invokes provider-native resume.
- **Session fork** — branch a Claude Code conversation into a new session and
  PTY without disturbing the original.
- **Prompt injection done right**: `session input` and `--input` use per-agent
  framing (bracketed paste, delayed submit) so multi-line prompts actually
  submit into Ink/TUI agents instead of being half-swallowed.
- **Provider-neutral observation**: read a bounded rendered screen, page through
  retained binary-safe output with exact runtime cursors, or wait up to eight
  seconds for state/activity/output changes without taking attach ownership.
- Rename sessions, attach arbitrary `key=value` metadata, and inspect
  everything as JSON.

**Multi-host, no central server**

- Every command takes `--host <name>`; session targets accept
  `<host>/<session-id>`. The CLI talks **directly** to each host's daemon —
  there is no coordinator, no SaaS, no state sync.
- Remote transport is one TCP listener per configured overlay, bound **only**
  to that provider's validated member address and port, never `0.0.0.0`.
  NetBird/WireGuard is the default provider; local access is an owner-only Unix
  socket.
- **Tokenless discovery**: `pohunek host discover` aggregates configured
  overlay peers and probes which run a reachable daemon. It needs provider-local
  state but not local `pohunekd`, and uses a short owner-private cache;
  `--refresh` re-probes.
  Status loading and peer probing have explicit bounded deadlines.
  `host inspect` queries live capabilities straight from the selected daemon.
- **Stable host identity and safe governance inspection**: every daemon keeps a
  stable opaque `HostId` and an owner-private local governance record. Inspect
  it with `pohunek host governance inspect <host> [--json]` to see the safe
  host ID, approval-key reference, explicit never-enrolled absence or one
  enrolled relay/owner/revisions/quarantine state. This is read-only: no relay,
  enrollment, ownership-transfer, or team-management command ships here.
  It adds no required configuration; the daemon manages the owner-private state
  under its existing XDG state directory.
- **Shell completion**: generate static Bash, Zsh, or Fish completion from the
  clap command tree. An explicit `--dynamic` mode adds bounded, failure-silent
  host and session-target lookup without starting a daemon.

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
  prompt) for launchers and scripts.
- `pohunek session diff` renders a unified diff of a session's worktree
  against its base — including untracked files — over the wire.

**Durable notification activity**

- Agent events (approval required, agent blocked, turn completed, session
  finished, errors) become **durable notification records** with lifecycle
  states (`unread → read → acknowledged → archived → deleted`).
- Fed by installed Codex/Claude hooks *and* daemon-side state projection, with
  source-priority dedupe, a debounce window that drops notifications the agent
  resolves itself, and resolve-on-resume so stale "blocked" entries disappear
  when the agent returns to working or its normal ready prompt.
- `pohunek notifications list|watch --all-hosts` fans out across every
  reachable host client-side. Per-kind/provider policy, automatic age retention,
  and physical JSONL compaction are daemon-enforced; unresolved actions and
  errors never expire automatically.

**Terminal UX**

- `pohunek setup config` installs the default attach config and the issue/PR
  prompt templates. The rofi/sway launchers are developed in
  [`zajca/pohunek-work`](https://github.com/zajca/pohunek-work).
- **Attach session menu**: raw terminal passthrough preserves native scrollback;
  `Ctrl-\` temporarily shows a composited dialog headed by the session's host,
  project, branch, name, and live state, with a menu (kill, terminate and
  delete, detach, new session in the same worktree, fork, rename), then restores
  the agent screen and raw passthrough when the menu closes.
- Attach auto-reconnects after a daemon restart to the same worker, PTY, child
  PID, and runtime generation. Retries use a minimum interval and consecutive
  attempt cap, while typed worker-stream failures stop immediately. A changed
  runtime generation is shown as explicit native recovery, not seamless
  continuation.

**Built to be driven by agents, not just humans**

- Automation commands have a versioned `--json` process envelope with exactly
  one `ok` or `err` document on stdout; diagnostics stay on stderr. Errors are
  structured (`class`/`code`/`msg` plus a recovery hint), and `subscribe`
  streams typed events over the same protocol. Session creation and input can
  read bounded UTF-8 payloads from stdin so prompts do not need to appear in
  argv, diagnostics, or logs.
- **Bundled agent skill**: `pohunek agent-skill` prints the complete
  agent-facing operating skill embedded in the binary — discovery, explicit
  targeting, JSON state reads, subscriptions, send-and-wait flows, and the
  safety boundaries — with `--json` emitting one envelope carrying the skill
  text and its sha256. It is fully local: no daemon contact, no filesystem
  reads, no network access; `--host` is accepted and ignored.
- **Universal assistant**: `pohunek assistant "how do I …"` launches a capable
  agent session preloaded with an offline knowledge bundle about pohunek
  itself and a redacted live snapshot of your hosts — self-hosted support for
  setup, project configuration, updates, and debugging.
- **SDKs — build your own GUI or client**: a Rust client crate
  (`pohunek-client`) and TypeScript packages (`@pohunek/protocol`,
  `@pohunek/sdk`, and `@pohunek/testkit`) speak the same versioned
  newline-delimited JSON protocol every client uses — nothing is private
  to any one client. Browsers use the node-free `@pohunek/sdk/browser` entry through
  a WebSocket relay. TS protocol types are generated from the Rust
  source of truth. If the bundled clients do not fit your workflow, wire up your
  own control plane on these SDKs instead of forking one.

## How it works

```text
  CLI / client (local)                CLI / client (remote)
       |                                   |
       | Unix socket                       | TCP over NetBird/WireGuard
       | (resolved private runtime root)   | (daemon binds ONLY to the 100.x iface)
       v                                   v
 +-----------------------------------------------------------+
 |                    host daemon (pohunekd)                  |
 |  public protocol | logical state | reconciliation | mesh   |
 +-----------------------------------------------------------+
       |
       | owner-private local worker protocol
       v
 +-----------------------------------------------------------+
 |  pohunek-sessiond: one native job per worker generation    |
 |  (systemd transient unit on Linux, launchd job on macOS)   |
 |  PTY master | child process | output ring | terminal state |
 +-----------------------------------------------------------+
       |
   Codex / Claude Code / Hermes Agent running in worker-owned PTYs
```

Each host is authoritative for its own sessions, projects, worktrees, and
notifications. Control traffic is newline-delimited JSON; attaching to a
session opens a **separate raw byte connection**, so JSON stays JSON and
terminal bytes stay bytes.

Durability is tiered and explicit: detach, terminal close, screen lock,
client restart, daemon restart, daemon crash, and daemon binary upgrade
preserve the same live PTY and child PID. Logout, a host reboot, user-manager
shutdown, or worker failure loses that runtime generation; after the next login
the logical session remains visible as `runtime.state=lost` with
`loss_reason=runtime_lost`, is never restarted automatically, and may be
recovered explicitly with `pohunek session resume` when it has valid native
recovery metadata.

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
- The private host approval signing key is stored separately from the safe
  governance response. Public inspection exposes only its verification-key
  reference, never a seed, signing key, proposal nonce, signature, or transfer
  outcome.
