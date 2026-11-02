---
kind: fixed
---
On Linux, `pohunek session rm` no longer refuses a stopped or lost session with `runtime_supervision_ambiguous` because of unrelated long-lived desktop processes whose environment cannot be read (`1Password-BrowserSupport`, `op daemon`, `ssh-agent`, another session's agent): a process outside the sessions slice that holds the session's worker, or in another session's worker unit, is now recognised as not belonging to the runtime. A process in the worker's own unit, in another generation of the session, or beside the unit under the sessions slice (tmux panes) still keeps the removal refused until it ends or `--accept-unconfirmed-cleanup` is given. macOS is unchanged.
