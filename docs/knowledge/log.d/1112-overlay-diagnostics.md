## Unreleased (2026-10-23, overlay diagnostics, #1112)

- `pohunek doctor` checks the configured overlay instead of NetBird
  unconditionally: one `<overlay>_cli` check per configured overlay, in the place
  `netbird_cli` held, so `netbird_cli` keeps its name. A provider CLI that is
  missing or untrusted is a `fail` with a fix hint when `overlays.toml` lists the
  provider; for the NetBird default of an absent file it stays a `warn`. The CLI
  doctor adds `overlay_config`, which fails on an unusable `overlays.toml` and
  warns "restart the daemon to apply" when the running daemon serves another
  overlay or port.
- `daemon.doctor` reports one `overlay_<overlay>_listener` check per configured
  overlay: its port and the listener address the provider reports. Checks are
  `{name, status, detail}`, so the wire shape is unchanged; a daemon of an
  earlier release reports none, and the CLI then does not compare.
- `host_unreachable` names the overlay, the address and the port that were
  dialled and, when the overlay is known, hints that the remote host's
  `overlays.toml` must list it. `ClientError::HostUnreachable` of
  `pohunek-client` gains `overlay`, `address` and `port` (a pinned Rust client
  adopts the change when it bumps its tag).
- `pohunek-hostcheck` no longer depends on `pohunek-netbird`; the doctor probes
  the configured overlay through the same provider transport the daemon and the
  client use, bounded by the overlay probe budget that now counts toward the
  `daemon.doctor` request timeout.
- `pohunek doctor` also flags a present `overlays.toml` when the daemon answers
  without any overlay check, as a daemon that predates the file does.
  `DaemonState::new` and `new_with_discovery` of `pohunek-daemon` take an
  `OverlaySource` argument.
