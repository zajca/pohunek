---
kind: changed
---
The daemon now stops its session registry's background work before it exits and refuses store writes made after that, and a worker no longer rewrites its journal after the terminal outcome is committed except to record the terminal acknowledgement.

The bound on that shutdown is the optional `deadlines.registry_shutdown_ms` key of `service.toml` (1 to 600000 ms, 5000 ms when absent).
