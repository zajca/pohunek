## Unreleased (2026-10-24, tailscale docs, #1112)

- New guide [Tailscale overlay](guides/tailscale.md): enabling the provider,
  installing the client on Linux and macOS, the selectors that name a peer,
  the `ShareeNode` rule, an ACL example and troubleshooting by error code.
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
