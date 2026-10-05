---
type: Concept
id: concept/agent-profiles
title: Agent profiles
description: Agent profiles resolve a user-facing agent name to a base runtime, program arguments, environment, and input rules on the daemon host.
source_kind: manual
intents: [setup, project, debug, help]
---

# Agent Profiles

An agent name can refer to a base runtime such as `shell`, `codex`, `claude`,
or `hermes`, or to a host profile resolved by the daemon. Profiles define the program,
arguments, optional environment entries, input behavior, resume behavior, and
manifest metadata for that host.

The assistant should run on a capable coding-agent runtime. The design ranking
prefers a user-defined `pohunek-assistant` profile when available, then `codex`,
then `claude`, then `hermes`, then other profiles based on those runtimes. The selected agent
must be reported to the user and remain overrideable.

Profile environment values are secret-bearing. They must not be copied into the
knowledge bundle, prompt, snapshot, logs, or documentation. See
[secrets](../safety/secrets.md).

Because the assistant reads knowledge from files, the launch path must verify
that the selected profile can read the materialized bundle and snapshot before
starting the session.

Profiles are not CLI-only. `host.inspect.runtimes` is the authoritative launch
inventory for any client, and launch pickers list its launchable base runtimes
and profiles. `supported_agents` remains a name-only compatibility summary and
is not sufficient for launch decisions. If runtime inventory is unavailable, a
client fails closed instead of inventing a fallback set.

`host.inspect.runtimes` is the availability and support decision point. Each
entry has a user-facing `agent`; optional `agent_base` is the `RuntimeRef` of the runtime
it launches. For Hermes, `version` and `supported` enforce the pinned 0.20.0 policy:
an unavailable executable omits them, while a detected unparseable or other
version reports `supported: false`. Launch a runtime only when `available` is true and `supported` is not
`false`; a present runtime with a version policy (Hermes) always reports
`supported`, so Hermes launches only on `supported: true`. The daemon re-runs that isolated, bounded version
probe for bare and profile-based Hermes immediately before `session.new` and
`session.resume`; a missing, unparseable, or non-pinned executable returns
`agent_runtime_unsupported` before it creates a worker, session, worktree, or
recovery write. The probe clears ambient user state and uses private temporary
HOME, Hermes, XDG, Python-cache, and working directories. Pohunek canonicalizes
the executable once and launches that exact absolute path without another PATH
lookup. A same-owner replacement of the canonical file between probe and exec
remains within the documented single-operator trust boundary. A runtime package can declare its own supported range with a data-driven probe
(`version_probe = { parser = "semver-v1", args, min, below }`); the daemon applies the
same sandboxed, bounded probe and the same `version`/`supported`/`agent_runtime_unsupported`
behavior to it, with only `PATH` inherited so interpreter scripts resolve (the Pi
package uses this). Keep legacy custom profiles usable when `agent_base` is absent,
but treat a present unknown base as display-only.

Hermes profiles use `base = "hermes"`. Their program and fixed arguments may
wrap the local terminal command, but they cannot enable fork semantics. The
compiled bare launch is exactly `hermes chat`; a valid native reference resumes
only as `hermes chat --resume <reference>`. The separately selected Hermes
operator plugin provides lifecycle hooks, typed tools, and a generated skill in
an isolated profile or custom absolute home; it never reads the profile's
`state.db`. See [Hermes operator](../guides/hermes-operator.md).

Resume and fork come from one native-session launch spec frozen into each
session at creation (see below). Clients read `SessionInfo.capabilities` rather
than branching on profile or base-kind names.
An unknown future base-kind string is presentation-only: it can be displayed
neutrally but cannot be launched, mutated, recovered, or persisted until the
daemon explicitly supports it.

## Switching between subscriptions

Several accounts of one agent (for example a work and a personal Claude
subscription) are several host profiles that point the agent at different config
homes. A runtime declares which variable relocates its home (`CLAUDE_CONFIG_DIR`
for Claude, `CODEX_HOME` for Codex); a profile with `base = "claude"` sets that
variable to an absolute path in its environment entries (a `~` is refused, not
expanded). The pieces fit together like this:

- **Launch.** The agent starts with the daemon's base environment (an allowlist
  of session variables) overridden by the profile's `[env]`, so the profile's
  variable decides which home the agent reads. A variable that only the daemon's
  own process has reaches no agent, unless the daemon's environment allowlist
  forwards it.
- **Hooks.** Native session ids, subagent lifecycle and notifications reach the
  daemon through hooks that live in the agent's config home, so every home an
  agent can run with needs them. `pohunek integration install --agent claude
  --profile work` installs into the home that profile launches with, and
  `--all-profiles` into every distinct home of the runtime (its own and each
  profile's). `integration status` and `doctor` take the same selectors, and
  their recovery commands name the profile of the home they describe. The
  installer resolves the home exactly as a launch does, so install, status and
  the launched agent always agree on the directory. Each home is its own
  transaction: a failure rolls that home back to its prior tree and leaves the
  others alone, and `--all-profiles` exits non-zero when any home failed.
- **Safety.** The profile selection is served by the local daemon only, because
  reports name directories derived from secret-bearing environment values, and
  a profile value that is not an absolute path is refused instead of expanded.
  Reports show directories, never environment values.

Resume freezing of the profile and the observer of external transcripts follow
in later changes of the same work; this page grows with them.

## Launch sources

A launch names its agent through a launch source. `session.new` on the owner's
host resolves an owner-local name: a host profile, else an installed runtime
id. A relay-selected launch resolves only a locally approved host profile by
name and approved revision; it can never name a runtime id, package, program or
argv. A bare runtime id fails with `agent_profile_not_found`, and a profile
edited after approval fails with `agent_profile_revision_stale`. The
revision is a keyed MAC under a host-local secret and covers the profile, its
manifest, its base runtime binding and the effective program and arguments; it
reveals no environment value, and an unreadable key fails closed with
`agent_profile_revision_unavailable`. No relay
caller exists yet; it depends on the locally approved host share of
[#82](https://github.com/zajca/pohunek/issues/82).

## Resuming and forking a profile-backed session

A session launched from a host profile freezes the profile's keyed revision
into its resume binding when it is created. Resume and fork resolve the profile
once, compare its current revision with the frozen one, and relaunch under that
single resolution only while they match:

- an edited profile (its text, `[env]`, detection manifest, runtime binding,
  program or arguments) fails with `agent_profile_changed`;
- a legacy session that never froze a revision fails with the same code;
- a profile that no longer resolves fails with `agent_profile_missing`;
- a host that cannot read its revision key fails closed with
  `agent_profile_revision_unavailable`, for `session.new` of a profile-backed
  session too. A session without a profile needs no key and is never compared.

This is what keeps a switch of account from taking effect silently: editing a
profile's `[env]` (for example the config home of a second subscription) makes
every existing session of that profile refuse to resume until the owner decides.
`pohunek session resume <id> --accept-profile-change` (and `session fork`) is
that decision. It relaunches under the current profile's env and detection
manifest and freezes its revision, so the next resume needs no flag. The launch
shape frozen at creation (program, arguments, resume spec, runtime pin) stays
as it was, and a deleted profile is refused even with the flag. The decision is
honored only on the local control socket; a remote caller gets
`agent_profile_change_local_only`. `plugin profile migrate` and
`package.bind_profile` rewrite the profile text, so existing sessions of that
profile need the flag once. Profile values never appear in these errors.

## Native recovery spec

A native-session launch spec states how an agent CLI resumes and optionally
forks one of its own conversations: the kind of native reference it consumes
(`id` or `path`), the resume argv, and an optional fork argv. Each argv has
exactly one reference slot. The daemon runs the argv directly and never invokes
a shell, so a reference containing spaces or shell metacharacters stays one
argument. A `path` reference must still be an absolute path and an `id`
reference must not begin with `-`; both checks run where the reference is
recorded and again when it is used.

Built-in specs (all `id` references):

| Base | Resume | Fork |
|------|--------|------|
| `claude` | `--resume <reference>` | `--resume <reference> --fork-session` |
| `codex` | `resume <reference>` | unsupported |
| `hermes` | `--resume <reference>` | unsupported |
| `shell` | none | none |

A profile without a `[resume]` table inherits its base spec. A `[resume]` table
is either `resumable = false` (no native recovery and therefore no fork) or a
complete spec; nothing is merged with the base:

```toml
base = "claude"
[resume]
reference_kind = "path"                 # "id" or "path"
args = ["--session", "{reference}"]     # resume argv, required
fork_args = ["--fork", "{reference}"]   # optional; absent means no fork
```

A runtime that declares `[native_reference] strategy = "assigned"` passes a
core-generated reference to the agent at launch, so a profile on that base
inherits the assignment (launch template and existence check) without a
`[resume]` table. A profile that restates `[resume]` on such a base is rejected
with `invalid_profile`; `resumable = false` switches recovery off and no
reference is generated. See the sessions concept page for the strategy and its
limits.

`fork_args` is accepted only on a base with compiled fork support (`claude`);
a `codex`, `hermes`, or `shell` profile that sets it is rejected, so those
bases keep returning `agent_fork_unsupported`. Runtime packages will declare
their own fork support later.

`{reference}` must be a whole argv token, appear exactly once per list, and is
never part of a larger string. The daemon rejects, with `invalid_profile` naming
the profile and the field, an unknown `reference_kind`, a missing `reference_kind`
or `args`, an empty list, a list without or with more than one `{reference}`, an
embedded form such as `--session={reference}`, any other brace in a token, an
empty or control-character token, `fork_args` without `args` or on a base without compiled fork support, and any
`[resume]` table on a `shell` base. A malformed profile fails before any worker,
session record, or worktree exists. Parse errors report the message and line
only, never profile environment values. The resolved spec is stored in the
session's recovery binding as `native_launch`; recovery and fork use that frozen
copy even after the profile changes. The binding also records the base runtime's
launch binding (runtime id plus where its definition came from). A profile whose
`base` is a valid runtime id that is not installed fails with
`runtime_not_installed`; a `base` that is not a runtime id fails with
`invalid_profile`. A recovery binding whose runtime is not installed stays in the
store, refuses resume and fork with `runtime_not_installed`, and works again once
the runtime is installed.

A runtime can also come from an installed runtime package (see
`concept/runtime-package-archive`, "Daemon host"). A session launched from a
package pins its archive digest; resume resolves exactly that digest, verified
again, never the currently selected version, a built-in or another package. A
pinned package whose root is modified or damaged refuses resume with
`runtime_incompatible` and keeps its binding.

## Package-bound profiles

A profile whose `base` is served by an installed runtime package binds that
package explicitly with two more top-level keys, set together or not at all:

```toml
base = "acme"                                   # the runtime id the package serves
package = "acme.runtime"                        # the package id
digest = "sha256:<64 lowercase hex digits>"     # the archive digest to launch from
```

- A profile over a built-in base (`shell`, `codex`, `claude`, `hermes`) has
  neither key; supplying them is `invalid_profile`, as is supplying only one.
- A profile over a package-served base without the keys is `invalid_profile`.
  Its message names the migration command
  `pohunek plugin profile migrate <name>`, which the daemon applies under the
  package lifecycle authority (`package.bind_profile`), writing the keys for the
  package version selected now with an atomic replace.
- With both keys the profile resolves from exactly that digest, never from the
  registry's current selection. `plugin update`, `plugin select` and a newer
  installed version do not change what the profile launches, while a bare
  runtime name keeps following the selection.
- A digest that is not installed is `runtime_not_installed`; a digest that
  belongs to another package or runtime, or whose root fails verification, is
  `runtime_incompatible`.
- A profile pin keeps its digest installed: `plugin uninstall` and the removal
  of a modified root are refused (`package_referenced`) while a host profile
  pins the digest, and `package.list`/`package.inspect` report it as
  `referenced`. A profile file that fails the owner-security checks, escapes the
  agents directory or does not parse pins nothing.

The profile revision covers the file text and the resolved launch binding, so a
migration (a changed `digest`) changes the revision and a relay-approved
revision fails with `agent_profile_revision_stale`, while an update of the
package does not touch the digest and leaves the approved revision valid.

Disabling a package stops fresh launches only. `session.new` through a pinned
profile, owner-local or relay-approved, fails with `runtime_not_installed`
because every fresh launch verifies that its package is enabled and its root
intact; a session already pinned to that digest still resumes and forks.
`host.inspect.runtimes` describes a pinned profile from its pinned definition
and reports `available: false` while its package is disabled, uninstalled or
fails verification.
