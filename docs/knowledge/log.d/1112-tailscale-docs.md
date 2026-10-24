## Unreleased (2026-10-24, tailscale docs, #1112)

- New guide [Tailscale overlay](guides/tailscale.md): enabling the provider,
  installing the client on Linux (macOS is not supported in 0.37, see #1583),
  the selectors that name a peer, the `ShareeNode` rule, an ACL example and
  troubleshooting by error code.
- Tailscale is supported on Linux in this release; **macOS is not**. The stock
  Apple-Silicon Homebrew directory `/opt/homebrew/bin` is admin group-writable
  and the trusted-executable policy refuses a `tailscale` binary there
  (`tailscale_cli_missing`). Tracked in
  [#1583](https://github.com/zajca/pohunek/issues/1583).
- Acceptance evidence: two Linux nodes on a Headscale v0.29.5 tailnet ran the
  direct owner path in both directions, and the NetBird default with an absent
  `overlays.toml` was checked unchanged. The redacted run is
  `docs/acceptance/1112-tailscale-overlay.json`; the macOS leg is recorded
  as failed under the limitation above, and its userspace sub-step did not run.
- NetBird is the default overlay and Tailscale a supported opt-in overlay in
  the product constraints, the architecture and the public API. The listener
  authenticates nothing beyond overlay reachability, so the overlay's policy
  (NetBird policies, tailnet ACLs) is the access control. A host runs one
  overlay.
- `pohunek doctor` names why a Tailscale node has no usable address
  (userspace networking, a backend that is not running, or no IPv4 address) in
  `tailscale_cli` and `overlay_tailscale_listener`; the
  `tailscale_listener_address_missing` message carries the same reason.
- The TypeScript SDK `hostUnreachable` message reads "could not open an overlay
  connection to host '<host>'" instead of naming NetBird, matching the Rust
  client and the protocol. Match on the `hostUnreachable` kind, not the text.
- Rust API: `OverlayError::ListenerAddressMissing` is a struct variant with
  `overlay` and an optional `reason`; code matching the old tuple form needs
  `ListenerAddressMissing { .. }`.
