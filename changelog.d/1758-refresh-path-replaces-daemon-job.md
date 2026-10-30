---
kind: changed
---
`pohunek service refresh-path` now replaces the daemon job together with the recorded search path, so the running daemon gets the new `PATH` at once instead of at its next version-changing upgrade. The command takes the new daemon's adoption verdict first, as an upgrade does (`--accept-runtime-loss` accepts the listed sessions), requires the daemon to restart, and restores the previous path and job when the replacement fails. An interrupted run is resumed by rerunning the command; `install` and `upgrade` refuse with `service_refresh_pending` while one that already replaced the job is pending. New error codes: `service_daemon_job_missing`, `service_refresh_config_changed`, `service_refresh_pending`.
