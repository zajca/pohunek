## Unreleased (2026-11-21, assistant package installed by the upgrade, #2500)

- `pohunek service upgrade` installs the assistant context package after a
  committed upgrade to a newer version: it asks the signed plugin index for the
  version listed for the new core and installs and selects it through the daemon,
  and `install-daemon.sh` does the same through the upgrade. The report carries
  `assistant.install` (`installed`, `not_published`, `held`, `failed`, `skipped`);
  none of them fails the upgrade. A disabled or unselected package stays as set,
  and `--no-assistant-package` skips the step. The release upgrade harness passes
  the flag, so it stays offline (also closes #2497). A fresh install still gets
  the package from `pohunek setup`, which the installer names.
