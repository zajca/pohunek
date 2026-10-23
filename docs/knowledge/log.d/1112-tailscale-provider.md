## Unreleased (2026-10-23, tailscale provider, #1112)

- `overlays.toml` accepts `providers = ["tailscale"]`. The new compiled crate
  `pohunek-overlay-tailscale` reads `tailscale status --json` through the
  trusted executable lookup, listens on the node's IPv4 address and routes to
  tailnet peers on IPv4 or IPv6. See [Remote hosts](guides/remote-hosts.md).
- The listener has no address, and discovery and resolution fail with
  `tailscale_state_unavailable`, unless Tailscale is `Running` on a kernel
  interface; userspace networking is not supported. Peers that are expired,
  outside the network map or shared in from another user are not selectable.
- The daemon refuses to bind an overlay address that is not a unicast address
  or differs from the address its provider reports at bind time.
- A provider listed in `overlays.toml` whose CLI is missing is retried by the
  daemon with its listener backoff instead of staying disabled until a restart.
  Without the file, the NetBird default is still disabled, as before.
- New error codes: `tailscale_cli_missing`, `tailscale_state_unavailable`,
  `tailscale_listener_address_missing`.
