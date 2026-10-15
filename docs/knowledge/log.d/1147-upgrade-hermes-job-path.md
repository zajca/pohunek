## Unreleased (2026-10-15, upgrade updates Hermes with the job's PATH, #1147)

- `pohunek service upgrade` updates the plugin of each marked Hermes home with
  the `hermes` executable on the daemon job's `PATH`: the recorded
  `service.toml` search path, or the systemd user manager's or launchd's
  `PATH` for a job without one. It is the same value the upgrade's runtime
  plan runs with, so the `PATH` of the shell that runs the upgrade does not
  decide which Hermes installation is updated.
