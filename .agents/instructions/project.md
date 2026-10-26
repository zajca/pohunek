# Project

`pohunek` is an **owner-first control plane for durable coding-agent sessions**
across the operator's own machines. A Rust daemon (`pohunekd`) owns the logical
session registry and public API on each host; one isolated
`pohunek-sessiond` worker owns each live PTY and agent process. The Rust CLI
(`pohunek`) drives the daemon locally over a Unix socket and remotely over the
configured overlay address (NetBird by default, Tailscale opt-in). This repository ships no user interface: every GUI,
web control center, and launcher is a separate client in
[`zajca/pohunek-work`](https://github.com/zajca/pohunek-work). The accepted
future direction adds an optional trusted team relay without replacing these
direct owner paths; it is not implemented yet.

It is pre-1.0 and experimental: wire shapes, config files, and on-disk metadata
may change freely. **Do not add backward-compatibility shims** unless asked,
except inside the upgrade window below.

### Upgrade window

The contract: release N stays compatible with release N-1 in exactly five
places, and nowhere else; everything outside this window keeps the no-shim
rule. The current state of each place is stated separately.

- **Live workers.** Contract and state: the private worker protocol accepts
  `PREVIOUS_VERSION` as well as `CURRENT_VERSION`
  (`crates/worker-protocol/src/version.rs`, today 7 and 8), so a daemon update
  leaves running sessions alone. A worker below the window is not adopted and
  the upgrade preflight reports it as `would_not_be_adopted`.
- **Worker journal.** Contract: the daemon reads the previous journal schema.
  State: the readable schemas are the explicit list
  `WORKER_JOURNAL_READABLE_SCHEMAS` (`crates/daemon/src/runtime/lifecycle.rs`);
  a schema is listed only when its layout names the worker generation, so today
  only the current `WORKER_JOURNAL_SCHEMA_VERSION` (4) is readable. Any other
  schema gets a typed reason and a WARN. The optional field `worker_spawn_id`
  is additive (no schema bump): the journal of an N-1 worker lacks it, and its
  lost runtime is swept without the fork-lineage proof. The same holds for the
  optional field `worker_coalitions`, which lets the sweep dismiss processes
  the kernel places in other coalitions than the worker's. The optional field
  `worker_control_group` (the worker's unified cgroup path on Linux) is additive
  the same way: its absence leaves every unreadable-marker process unproven. The optional outcome
  field `output_forced_closed` (also on the private `ExitStatus`) is additive
  the same way: an N-1 worker's outcome lacks it and reads as `false`. The
  optional claim fields `agent`, `origin` and `reported_session` of a forwarded
  active identity and the optional `active_identity_revision` (read as 0) are
  additive without a schema bump too: an N-1 reader of such a journal sees a
  provider-only active claim with no native reference.
- **Public-protocol clients.** Contract: a protocol change keeps the previous
  version served through adapters on both sides of the connection. An adapter
  translates shape only. A semantic change raises `MIN_PROTOCOL_VERSION` too
  and is an announced break; the oldest adapter is deleted on the next bump.
  State: the daemon accepts `3..=4` (`MIN_PROTOCOL_VERSION` is
  `PROTOCOL_VERSION - 1`, adapter in `crates/protocol/src/compat/v3.rs`) and the
  Rust client, the CLI and the TypeScript SDK advertise the same window
  (`CLIENT_PROTOCOL_VERSIONS` is `SUPPORTED_PROTOCOL_VERSIONS`), translating
  through the same adapter (ported to TypeScript in `sdk/ts/sdk/src/compat.ts`,
  tested against the same recordings) when the daemon answers in N-1. A method
  new in N fails on the client with `daemon/daemon_protocol_too_old` before it
  is sent ([#527](https://github.com/zajca/pohunek/issues/527)). A new value of
  an existing enum on the same version is decoded tolerantly only by the client
  (`protocol::tolerant`, hidden `Unknown` variant); each enum's policy, and the
  bump-only ones, are in `docs/architecture.md` "Unknown enum values from a
  newer daemon".
- **Accepted upgrade risk.** Contract: the CLI remembers which at-risk verdicts
  an upgrade accepted, and the next gate treats a verdict as previously
  accepted only when the daemon of the release being installed reproduces the
  digest recorded with it. State: the preflight report's optional
  `acceptance_scope` (`v1:` and a digest, frozen by a golden test in
  `crates/daemon/tests/upgrade_preflight.rs`) and the CLI's
  `<state>/pohunek/service-accepted-risks.json` (schema 1, replaced by every
  completed upgrade, bound to the installed version, namespace and metadata
  store). It fails closed in both directions: a missing, mismatched, unknown or
  unreadable scope, scheme, schema or ledger means not accepted, and there are
  no reader shims. A change to the digested facts needs a new scheme name.
- **Persisted daemon state.** Contract and state: `metadata.jsonl` migrates
  from any older kept schema, because skipping releases is normal with
  `update-pohunek`. A persisted shape change requires a schema bump and a
  migration step.

Every persisted record that cannot be migrated or adopted is logged at WARN,
surfaced to the operator, and carries a recovery hint. A `metadata.jsonl` line
no loader accepts stays in the file: every rewrite re-emits it unchanged after
the parsed records, and the recovery hint is to remove it from the file. A store
newer than the binary, or older than every kept migration, makes the daemon
refuse to start and leaves the store untouched.

The operator-facing gate of the window is the upgrade preflight: `pohunek
service check|upgrade` run `pohunekd upgrade-preflight` of the new archive
before any effect. It judges the dry-run store migration and every live session
(`adoptable`, `would_lose_recovery`, `would_not_be_adopted`) from the store,
worker journals and process table only (`crates/service-config/src/preflight.rs`,
`crates/daemon/src/session/reconcile/upgrade_preflight.rs`,
`crates/daemon/src/store/dry_run.rs`); it never connects to a worker socket and
never writes. Sessions at risk refuse the upgrade unless `--accept-runtime-loss`,
the single loss-accepting flag, is given; a refused store never is overridable.

`scripts/release` compares these constants with the previous tag and requires
a release-notes line for each changed one (see the `release` skill).

#### Store schema and the shape guard

Every line of `metadata.jsonl` (session, resume, worktree and project records)
carries `schema_version`; a line without it is schema 1. The current schema is
`STORE_SCHEMA_VERSION` in `crates/daemon/src/store/schema.rs`. The daemon runs
`store::migrate_at_startup` in `crates/daemon/src/main.rs` before the session
registry exists, and copies the store to `<store>.pre-schema-<old>` before the
first migrating write.

The guard test in `crates/daemon/src/store/shape_guard.rs` compares the
serialized field set and the serialized value set of every persisted enum of
every persisted record kind, including the nested `SessionInfo` protocol type,
with the snapshot
`crates/daemon/src/store/fixtures/shape/schema-<STORE_SCHEMA_VERSION>.txt`
(`kind.path` key lines and `kind.path=value` enum lines). It fails when a field
is added, removed or renamed, when an enum value is added, removed or renamed,
and when a string-valued field is neither a registered enum path nor listed free
text, all without a schema bump, and when an older schema has no migration step.
A new enum value needs the bump because an older daemon rejects the line or
degrades it to a fallback variant. An
`Unknown` variant contributes no value. A protocol field change in `SessionInfo`
trips the guard on purpose: the field lands in `metadata.jsonl`. To resolve a
failure:

1. bump `STORE_SCHEMA_VERSION`;
2. add a step for the previous schema to `MIGRATIONS` in `schema.rs`;
3. add `fixtures/shape/schema-<new>.txt` (the failure prints the key and value
   set) and register it in `SHAPE_SNAPSHOTS`;
4. freeze a store written by the previous release as a fixture under
   `crates/daemon/src/store/fixtures/` and test that it migrates.

`governance.json` and `identity.json` carry their own format versions
(`GOVERNANCE_FORMAT_VERSION`, `IDENTITY_FORMAT_VERSION` in
`crates/daemon/src/host_state/records.rs`), and each reader accepts exactly one.
The sibling guard `crates/daemon/src/host_state/records/shape_guard.rs` pins the
key and enum value sets of both records to
`crates/daemon/src/host_state/fixtures/{governance,identity}-<N>.txt`. A failure
means: bump the format constant, make the reader accept the previous format, and
add the new snapshot. `notifications.jsonl`, `policy.json` and the other
daemon-owned documents are not covered by these guards.

Authoritative design lives in `docs/architecture.md` (it wins over `idea.md`).
Hard constraints, decided on purpose — respect them in every change:

- **Direct owner operation stays first-class.** Standalone and direct-overlay
  clients keep talking to each host daemon without a relay dependency. Their
  trust boundary remains owner-only socket/file permissions plus the configured
  overlay. NetBird is the default provider and Tailscale a supported opt-in
  provider; a host runs one overlay.
- **The owner browser path stays first-class.** The web control center is an
  external client (`zajca/pohunek-work`) that reaches the owner protocol through
  a private local/NetBird gateway. It is not replaced by the relay and must
  never become a second team-auth or relay-routing authority.
- **The optional team relay is additive.** The accepted design introduces one
  trusted Rust `pohunek-relayd` authority for teams, end-user authorization,
  routing, audit, and quotas. Each host remains authoritative for its PTYs,
  processes, worktrees, session origin, and locally approved `HostShare`
  ceilings. The first release trusts collaborators at the host Unix-account
  boundary: relay ACLs constrain relay actions but are not workload isolation;
  [#88](https://github.com/zajca/pohunek/issues/88) owns that later boundary.
  See the [accepted RFC](../../docs/design/team-relay-control-plane-rfc.md), especially
  its identity, recovery, scheduling, catalog, audit, and dependency sections,
  and [#85](https://github.com/zajca/pohunek/issues/85); neither the relay binary
  nor team mode is shipped yet.
- **PTY/TUI-first.** Agents run in real terminals (Codex, Claude Code, and the
  pinned local Hermes Agent runtime are first-class). Not a re-rendered control
  plane.
- **Remote owner transport is direct over the configured overlay** (NetBird by
  default, Tailscale opt-in), never SSH bridging. Relay
  transport is the separate host-initiated path defined by the accepted RFC.
- **Core ships no UI.** The deliverables are the daemon (`pohunekd`), the
  session worker (`pohunek-sessiond`), the CLI (`pohunek`), the relay
  (`pohunek-relayd`), the Rust crates, and the TypeScript SDK workspace
  (`sdk/ts/`). No GUI, web frontend or backend, desktop launcher, or UI
  packaging belongs here; they live in `zajca/pohunek-work`.
- **The contract boundary is public contracts only.** A client reaches core
  through the CLI with `--json` or through the public protocol via the
  versioned SDKs: the Rust crates pinned by git tag and the TypeScript SDK
  release tarballs pinned by URL and integrity. The core crates a Rust client
  links (`client`, `protocol`, `paths`, `platform`, `prompt`, `knowledge`,
  `assistant`, `netbird`, `overlay`, `overlay-tailscale`) are a pinned API, not
  a stable one: they stay pre-1.0 with no back-compat shims, and a client
  absorbs breaking changes when it bumps its pinned tag.
- **UIs pin the core release they were built against.** A Rust or
  TypeScript client advertises the window `CLIENT_PROTOCOL_VERSIONS` and reaches
  an N-1 daemon through its adapter; a client two releases newer than its daemon
  is refused (`daemon/version_mismatch`). A daemon
  of release N additionally serves the clients of release N-1 through the
  protocol window (`docs/architecture.md` "Protocol versioning"), which bounds
  how long a UI release trails a core update. A protocol surface that only UI
  clients call (`host.discover`, `worktree.remove`, and the UI use of
  `subscribe`) is still a public obligation: core keeps server-side contract
  tests for it, listed in `docs/public-api.md` ("External clients"). Core adds
  no CLI command only to serve a UI.
- **Plugins live outside core.** Every new runtime package or workflow plugin
  starts outside `zajca/pohunek`: an official plugin in `pohunekd/plugin-<name>`,
  a third-party plugin in its author's account (`<owner>/pohunek-plugin-<name>`).
  Do not add a directory under `runtime-packages/` or `compat/`, a package job
  to `.github/workflows/ci.yml`, or a test about one plugin to a core crate.
  The official runtimes still under `runtime-packages/` (Pi, Codex, Claude Code,
  Hermes) are changed in place until their move (#1609) lands. Core owns what
  plugins build on: the descriptor and manifest schemas, `runtime_api` and the
  runtime features, the integration handlers, the plugin kit, the reusable
  plugin workflows, the index format and verifier, and the release catalog.
  A plugin that needs a new core capability gets it in a core pull request
  first, behind a runtime feature; the plugin adopts it against the nightly
  kit. There is no paired core-and-plugin merge. A plugin is admitted, updated
  or revoked by a reviewed pull request to `pohunekd/pohunek-plugin-index`; no
  core release is needed for that. A core release bundles the official set of
  its day from `packaging/official-plugins.lock.json`, a snapshot of the index,
  and proves every locked plugin against its exact artifacts. Naming and the
  full rules are in `docs/knowledge/guides/plugin-repositories.md`. The
  template, the plugin kit, the index and the release snapshot are available
  from a later release (#1613-#1616, #1618; "What is available when" in that
  guide); until then no new plugin is added here, and a plugin installs by
  digest.
- **Issue/PR providers (Linear, GitHub) live only in `zajca/pohunek-work`,
  never in core** (neither the daemon, the CLI, nor this repository's scripts).
- **Protocol today:** public protocol v4 (the daemon also accepts v3 through the
  protocol window) is owner-only newline-delimited JSON
  over a Unix socket (local) and TCP on configured overlays (remote); attach
  uses a separate raw-byte connection per PTY. [#70](https://github.com/zajca/pohunek/issues/70)
  owns the coordinated v4 host-link cutover; do not describe relay protocol as
  shipped before that issue lands.
