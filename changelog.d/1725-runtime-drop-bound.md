---
kind: fixed
---
The daemon now exits inside `daemon_exit_timeout_ms` even when a blocking store write never returns: it drops its async runtime through a 1000 ms bound, logs a warning and exits, and the registry shutdown log names the stuck write.
