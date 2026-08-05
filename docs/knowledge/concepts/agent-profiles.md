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

Profiles are not CLI-only. `host.inspect.runtimes` is the authoritative native
GUI launch inventory, and the Start-session and Dispatch pickers list its
launchable base runtimes and profiles. `supported_agents` remains a name-only
compatibility summary and is not sufficient for launch decisions. If runtime
inventory is unavailable, the GUI fails closed instead of inventing a fallback
set (see [GUI: Session Launch](../guides/gui.md#session-launch)).

`host.inspect.runtimes` is the availability and support decision point. Each
entry has a user-facing `agent`; optional `agent_base` identifies its compiled
adapter. For Hermes, `version` and `supported` enforce the pinned 0.20.0 policy:
an unavailable executable omits them, while a detected unparseable or other
version reports `supported: false`. Launch Hermes only when `available` and
`supported` are both true. The daemon re-runs that isolated, bounded version
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
only as `hermes chat --resume <reference>`. M2 does not install a Hermes plugin,
lifecycle hook, or skill and never reads the profile's `state.db`.

Resume and fork are independent capabilities frozen into each session at
creation. A profile may disable a capability supported by its base adapter, but
it cannot invent support the adapter does not have. Clients read
`SessionInfo.capabilities` rather than branching on profile or base-kind names.
An unknown future base-kind string is presentation-only: it can be displayed
neutrally but cannot be launched, mutated, recovered, or persisted until the
daemon explicitly supports it.
