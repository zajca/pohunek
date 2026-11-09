---
kind: removed
breaking: true
---
Session fork is removed: `pohunek session fork`, the attach menu entry, the `session.fork` method, the Hermes `pohunek_session_fork` tool, the `forkable`, `fork_args` and `fork_launch_args` fields of the runtime package descriptor and the `[fork]` table of runtime descriptors. `capabilities.fork` stays on session listings and is always `false`. A descriptor, host profile or stored session record written by an earlier release that still carries fork data loads and the fork data is ignored; a descriptor or host profile also logs a warning.
The session store moves to schema 17 on the first start; a daemon of an earlier release cannot open a store migrated by this one. A session that was fork-capable and is at risk in an upgrade preflight asks for the risk acceptance once more.
