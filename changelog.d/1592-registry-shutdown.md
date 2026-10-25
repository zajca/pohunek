---
kind: changed
---
The daemon now stops its session registry's background work before it exits and refuses store writes made after that, and a worker no longer rewrites its journal after the terminal outcome is committed except to record the terminal acknowledgement.
