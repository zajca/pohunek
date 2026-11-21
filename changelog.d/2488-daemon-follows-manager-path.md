---
kind: fixed
---
On a systemd user session the daemon now finds agents that the login environment adds to the user manager's `PATH` after the daemon started (for example mise shims after a reboot), without a restart. It re-reads the manager's `PATH` on each request when its unit sets no `PATH` of its own. ([#2488](https://github.com/zajca/pohunek/issues/2488))
