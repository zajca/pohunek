---
type: Concept
id: concept/agent-profiles
title: Agent profiles
description: Agent profiles resolve a user-facing agent name to a base runtime, program arguments, environment, and input rules on the daemon host.
source_kind: manual
intents: [setup, project, debug, help]
---

# Agent Profiles

An agent name can refer to a base runtime such as `shell`, `codex`, or `claude`,
or to a host profile resolved by the daemon. Profiles define the program,
arguments, optional environment entries, input behavior, resume behavior, and
manifest metadata for that host.

The assistant should run on a capable coding-agent runtime. The design ranking
prefers a user-defined `pohunek-assistant` profile when available, then `codex`,
then `claude`, then other profiles based on those runtimes. The selected agent
must be reported to the user and remain overrideable.

Profile environment values are secret-bearing. They must not be copied into the
knowledge bundle, prompt, snapshot, logs, or documentation. See
[secrets](../safety/secrets.md).

Because the assistant reads knowledge from files, the launch path must verify
that the selected profile can read the materialized bundle and snapshot before
starting the session.
