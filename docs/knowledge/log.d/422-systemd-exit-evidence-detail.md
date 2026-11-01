## Unreleased (2026-11-01, systemd exit evidence detail, #422)

- `ExitEvidence` of the systemd backend gains `oom_kills`, `managed_oom_kills`, `memory_peak` and `exited_at`, read from the service interface inside the `InvocationID` bracket of `exit_evidence`. Each property is read on its own: an unknown property or the manager's `UINT64_MAX` leaves it absent and the `Result`/`ExecMain*` verdict intact.
- systemd 251+ ends a unit that `systemd-oomd` killed with `Result=oom-kill`, the same result as a kernel OOM kill. Only the counters (`OOMKills` and `ManagedOOMKills`, systemd 259+) distinguish the killer; on 255-258 the killer stays unknown while `MemoryPeak` still shows.
- The real-systemd lane installs and starts `systemd-oomd`; the scenario for an oomd-killed worker fails with a stated precondition when it is not running. No wire, store or daemon change in this step.
