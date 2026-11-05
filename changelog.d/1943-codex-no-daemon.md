---
kind: changed
---
Codex sessions are launched with `--no-daemon` by the `codex-hook-v1` integration handler on every create and resume, profile sessions and sessions created earlier included, so two Pohunek Codex sessions on one `CODEX_HOME` no longer share Codex's managed app-server daemon: each session's hooks report to its own worker, and stopping one session no longer strands the other. The Codex package (1.5.0) now supports Codex from 0.164.0, the first release verified with the flag.
