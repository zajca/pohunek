---
kind: fixed
---
On macOS, `pohunek service uninstall --stop-sessions` no longer fails with `loaded job has no private definition` when a stopped session's worker job is retired while the uninstall inspects it; a job that vanished during inspection (also for the daemon agent and plugin unit status) is treated as gone.
