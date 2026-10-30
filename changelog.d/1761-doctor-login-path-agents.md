---
kind: added
---
`pohunek doctor` on macOS reports `service_search_path_agents` (`warn`) when `claude` or `codex` resolves through your login shell's search path but not through the one recorded in `service.toml`, which the daemon runs with; the detail names the program and the remedy `pohunek service refresh-path --dry-run`. The probe is bounded, takes no transaction lock, and reports nothing when the login shell fails or hangs, on a host that records no path, or without an installation.
