---
type: SafetyPolicy
id: safety/trust-model
title: Assistant trust model
description: Safety rules for a capable assistant that can inspect and edit host and repository files through an ordinary agent session.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Assistant Trust Model

The assistant is intentionally capable. It runs as an ordinary selected agent
session and can use that agent's normal file and command tools. Safety comes from
clear launch boundaries, public-safe knowledge, redacted snapshots, explicit
configuration review, and existing Pohunek daemon and filesystem controls.

The assistant must:

- Explain intended config edits before making them.
- Preserve user edits unless explicitly asked to overwrite.
- Prefer structured `--json` inspection commands for state.
- Verify changes before claiming they work.
- Keep remote confirmation behavior intact.
- Treat hooks as executable code requiring explicit review.
- Keep daemon-managed hook assets inside an explicit owner-private config-root
  trust anchor. Status must inspect each asset through one no-follow descriptor
  for type, effective-UID ownership, mode, and content, and must reject any
  effective-UID-foreign, symlinked, special, or group/world-writable asset. From
  the selected config root through the direct asset parent, every directory must
  be real, effective-UID-owned, and not group/world writable. Ancestors above the
  selected root are outside this chain so normal home/XDG paths are not rejected
  solely because a shared system ancestor exists.
- Treat Claude `settings.json`, Codex `hooks.json`, and Codex `config.toml` as
  owner-private registration authority. Status reads metadata and bounded
  content from one no-follow descriptor and requires a regular effective-UID-
  owned file without group/world write access. A missing Claude `hooks/` child
  below a trusted config root is safe absence; installation creates it with
  exact mode `0700` regardless of inherited umask, removes that newly created
  directory if mode enforcement or safe opening fails, never chmods an existing
  real user directory, and rejects unsafe path shapes. Codex trust identity
  covers a canonical single-handler managed group, so sibling handlers cannot
  inherit the managed trust record. Its managed trust-key set is exact; stale
  managed tables are removed on reinstall, while scalars anywhere in the
  managed trust namespace require configuration repair.
- Require `CLAUDE_CONFIG_DIR` and `CODEX_HOME` to resolve to absolute UTF-8 paths
  before registration commands are constructed. Before any integration install
  mutation, open and validate all existing config and hook parents within the
  explicit trust anchor. Perform temporary-file creation, mode setting, and
  replacement relative to those open directory descriptors so concurrent name
  swaps cannot redirect writes. Preserve safe existing provider-file modes and
  create new registration files as owner-readable and owner-writable only.
- Avoid weakening owner-only profile checks, name guards, path containment, or
  remote safety gates.
- Treat host governance inspection as a read-only owner operation. The stable
  protocol `HostId` is not a daemon route selector, and safe output must retain
  explicit never-enrolled absence rather than synthesizing an owner or revision.
  Never expose or log approval signing material, proposal/outcome coordinates,
  nonces, signatures, retired state, or relay credentials. A governance failure
  is unavailable state, not permission to reuse a previous snapshot.
- Treat worker control sockets and journals as owner-private runtime authority.
  Never proxy a worker endpoint over NetBird, unlink a failed socket without
  proving unit inactivity and exact identity, or edit worker/runtime ids by
  hand.
- Treat kernel peer identity as the only source of a caller's process identity
  on the private worker paths. It is read from the accepted socket on both
  Linux and macOS before a request is parsed, and re-read before each decision
  that grants authority — every request on a leased control connection and
  every frame of a live attach stream, because a connection outlives the moment
  it was authorized. A peer that exited without being reaped does not count as
  live. A request field never supplies it, the owner alone
  never authorizes a claim that needs a process id, and a peer the kernel
  cannot attest — including one with no process id — is rejected rather than
  downgraded. A private identity report is accepted only from the process it
  names or from a descendant of that process; a sibling in the same session is
  not enough. This binds a report to its reporter, not to the account: an
  owner-private socket is still not a sandbox against arbitrary commands run
  under the same Unix account, and in a shell session most of what the operator
  runs is inside the managed process tree by construction. The public
  `session.report_native_id` fallback has no peer binding at all and relies on
  its runtime, ordering, expiry, and provider rules.
- Never copy worker journal or structured-log diagnostics into shared reports
  without review. Journals intentionally omit prompt, input, terminal, and
  environment bytes; preserve that boundary when adding diagnostics.
- Preserve paired `origin_session_id`/`origin_daemon_id` request markers on
  ordinary, subscription, and dedicated SDK connections. The daemon uses them
  to deny exactly `session.stop`, `session.resume`, `session.remove`,
  `session.fork`, `session.resize`, `session.set_metadata`, `session.rename`, and
  `session.input` when they target the session hosting the caller. Do not strip
  or forge them to bypass `plugin_self_target_denied`. This is a narrow
  confused-deputy guard within the owner trust boundary, not per-session
  authentication or a general mutation policy. The lifecycle reports
  `session.report_agent`, `session.release_agent`, and
  `session.report_native_id` are deliberately allowed to target their own
  session; hooks require that path, and the public native-id report is the
  necessary local fallback when the owner-private worker claim cannot be
  delivered.
- Treat `notification.create` like every other control method: it is guarded by
  the owner-only daemon socket, not by per-session authentication. Any same-user
  process that can reach the socket can create notifications and influence
  attention dedupe within the single-operator trust boundary. A supplied
  `session_id` is shape-validated so it is bounded and contains no control
  characters, but it is not cryptographically authenticated to a session.
- For Hermes, never read, copy, or modify `HERMES_HOME` or `state.db` to infer
  a resumable session. Use only a valid reported native reference. Programmatic
  Hermes input is restricted to bounded text with LF/tab as the only controls
  and is denied while owner approval is visible; do not bypass that guard with
  raw attach bytes. Use only the installed typed tool surface, an explicit host
  allowlist, and the selected `read_only`, `manage`, or `full` access mode.
  The plugin policy is an owner-private delegated-tool guardrail, not a
  same-user sandbox; the daemon's exact eight-method origin-session denial is
  authoritative. Ask a human to attach when typed model control is insufficient.

The assistant may write host or repo configuration when that is the requested
task, but it must stay inside the user's requested scope and respect the
boundaries in [secrets](secrets.md) and [repo `.pohunek/`](repo-pohunek.md).

## Accepted team-relay boundary

The [optional team relay](../concepts/team-relay.md) has an implemented reduced
foundation: PostgreSQL-backed fencing and recovery, protected local
provisioning, generic OIDC authentication, bounded HTTPS account and credential
lifecycle, provider-neutral account linking, and a native HTTPS/keyring CLI. Do
not invent host-link, routing, attach, team-administration,
provider-verification, or team browser commands, configuration keys, protocol
fields, or recovery steps.
Current protocol-v3 local, overlay, and Bun browser paths remain one owner trust
domain and remain supported alongside the foundation. The relay has no local
mode. Owner and team browser surfaces must not exchange credentials, state, or
silently fall back between their explicit origins and API adapters.

[Account linking](../concepts/team-relay.md) is an authority change, so treat it
with the same care as a credential. A relay identity is exactly the issuer plus
the immutable subject; never treat an email address, display name, or any other
provider profile attribute as proof that two accounts are the same person. A
link requires both the current relay actor and the new identity to prove
themselves in one audited transaction, and it is completable only through the
channel that opened it. A link or unlink takes effect on current credentials and
browser sessions immediately: an unlink revokes everything derived from the
removed identity and cannot be undone by retry, restore, or cached provider
state. An account always keeps at least one identity. Linking is an HTTPS-only
relay surface with no CLI subcommand, so do not invent one; and never repeat a
link transaction's one-use possession value in output, a log, an issue, or a URL.

The implemented foundation authenticates generic OIDC subjects and credentials;
future relay work must preserve these independent authorization responsibilities:

- The relay authenticates and authorizes human principals, service accounts,
  teams, groups, roles, and session ACLs.
- `pohunekd` authenticates the enrolled relay and enforces the current local
  `HostShare` ceiling, including projects, worktree roots, agent profiles,
  operations, resource limits, and immutable session origin. End-user identity
  is not a daemon authorization input.
- Local and direct-overlay sessions are never relay-visible or
  relay-controllable. A claimed session ID or relay-supplied metadata must not
  bypass the origin check.
- Every share request and ownership transfer grants nothing until the required
  local host confirmation. Relay-side state cannot expand host authority.
- Each relay operation is bound to one active share and its current revision;
  permissions from different shares are never composed for one operation.

The planned relay uses Keycloak as the reference broker for Google and GitHub,
but the implemented foundation remains provider-neutral. Verified external
eligibility is deferred to [#92](https://github.com/zajca/pohunek/issues/92);
it will be evidence, not an editable profile attribute, and it expires no later
than 60 minutes after authoritative upstream verification. Token refresh,
activity, restart, unknown state, or provider failure cannot extend it.
Confirmed removal and local revocation cancel affected relay access, including
idle streams, without stopping host sessions. RFC §13 is normative.

The relay is explicitly trusted with transient terminal plaintext and all
authority granted by active shares. A compromised relay can exercise that
entire union, even though application RBAC gives an ordinary infrastructure
administrator no implicit session access. Never describe the relay as
end-to-end encrypted from its operator. Relay persistence and telemetry must
exclude PTY bytes, input, prompts, terminal snapshots, file contents, and raw
secrets.

The first relay release trusts collaborators at the daemon owner's Unix-account
boundary. Direct-host agent profiles run under that account; relay API ACLs and
`HostShare` limits reduce relay authority but are not command or hostile-workload
isolation. Container and VM-backed execution is separate post-release work in
#88.
