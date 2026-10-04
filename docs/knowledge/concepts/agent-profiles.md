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
entry has a user-facing `agent`; optional `agent_base` identifies its compiled
adapter. For Hermes, `version` and `supported` enforce the pinned 0.20.0 policy:
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
remains within the documented single-operator trust boundary. Keep legacy custom profiles usable when `agent_base` is absent,
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
