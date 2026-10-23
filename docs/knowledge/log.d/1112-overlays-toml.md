## Unreleased (2026-10-23, overlays.toml, #1112)

- A host's overlay provider and remote control port come from the optional
  owner-private file `<config>/pohunek/overlays.toml` (`providers`, `port`).
  Without it the host works over NetBird on port 18722, as before. The CLI, the
  daemon and external Rust clients share one loader in `pohunek-client`; the
  daemon validates the file before it takes a lock, migrates its store or adopts
  a worker. The guide [Remote hosts](guides/remote-hosts.md) describes the
  file.
- One overlay per host: `providers` takes exactly one known id (`netbird`), and
  several entries fail with `overlay_configuration_invalid`, pointing to #1113.
- CLI `--json` error code change: an unusable `POHUNEK_REMOTE_PORT` reports
  `overlay_configuration_invalid` instead of `netbird_configuration_invalid`,
  and every problem with `overlays.toml` or its directory (permissions, owner,
  symlink, size, encoding, syntax, unknown key, port range) reports the same
  code. `netbird_configuration_invalid` is no longer emitted.
- A service-started daemon never received `POHUNEK_REMOTE_PORT`, so it bound
  port 18722 whatever the CLI dialed; the port in `overlays.toml` reaches both.
  Upgrade the daemon of a remote host and restart it before setting `port`
  there: an older daemon ignores the file.
- Help text of `host` and `--host` names the configured overlay instead of
  NetBird, and `host_unreachable` of the CLI client reads "could not open an
  overlay connection".
