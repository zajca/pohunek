## Unreleased (2026-10-24, acceptance harness stages dbus-user-session, #1112)

- The Debian 13 genericcloud image has no `dbus-user-session`, so its systemd
  user manager has no session bus and `pohunek service install` fails with
  `supervisor operation connect is unavailable`. The acceptance harness now
  stages the pinned package offline (sha256-verified, snapshot.debian.org
  primary), restarts the user manager and waits for `dbus.socket` before
  installing the service; `docs/install.md` names the prerequisite for minimal
  Debian and cloud images. No product code changes.
