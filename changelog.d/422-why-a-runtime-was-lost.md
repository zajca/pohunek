---
kind: added
---
A lost session runtime now says why. `runtime.exit` of an OOM-killed worker names the memory killer (`oom_killer`: `kernel` or `systemd_oomd`, only when exactly one is evidenced) and carries the unit's peak memory (`memory_peak_bytes`) and end time (`ended_at`). A loss with no service-manager verdict carries `runtime.loss_cause`: `user_manager_lost` when the systemd user manager that held the worker is gone, `host_restarted` after a reboot. The fields are additive on protocol 4 and absent on protocol 3; the metadata store moves to schema 15 (a no-op migration), so a release that predates it refuses the store and a rollback restores the `metadata.jsonl.pre-schema-<previous>` backup.
