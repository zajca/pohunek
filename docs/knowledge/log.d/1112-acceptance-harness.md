## Unreleased (2026-10-24, Tailscale overlay acceptance harness, #1112)

- `scripts/acceptance/tailscale-overlay` is the manual D9 acceptance run of the
  Tailscale overlay provider: two qemu VMs with Headscale and a kernel-TUN
  `tailscaled` (D4 in both directions over the CLI, attach and the SDK control
  path), the absent-`overlays.toml` NetBird default on a throwaway daemon, and a
  non-root macOS leg (D6 trusted-CLI lookup and the userspace-networking
  fail-closed path). `scripts/acceptance/overlay_evidence.py` interprets the
  recorded steps into `docs/acceptance/1112-tailscale-overlay.json`; the
  procedure, pins, resource budget and network exposure are in
  `docs/acceptance/README.md`. Nothing here changes product behaviour.
- `crates/client/examples/overlay_probe.rs` is a public-API consumer of
  `pohunek-client` (registry from configuration, `resolve_host_target_in`,
  `Client::connect_with_registry`, handshake, `session.list`, `daemon_doctor`,
  `session_screen`). The harness builds it static for
  `x86_64-unknown-linux-musl`; it is not shipped in a release archive.
- The harness never removes a macOS root it did not create (one flag per root,
  set only after that root's own `mkdir`), runs Homebrew without auto-update or
  cleanup, treats an unexpected `brew list` answer, a Mac step timeout or a
  different Tailscale version as inconclusive, and records the `netbird` and
  `macos` legs through an allow-list so the public evidence carries no owner
  peer names, domains or Mac paths.
