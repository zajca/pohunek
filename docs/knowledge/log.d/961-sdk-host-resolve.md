## Unreleased (2026-10-20, TypeScript SDK host resolution, #961)

The daemon answers `host.resolve`, which maps a host route to this machine or to
one pinned overlay peer with the selector rules of the CLI and the Rust client;
it is served on the local Unix socket only. The TypeScript SDK exposes
`resolveHost` and `connectHost`, and `attachRaw` reaches remote hosts through
them.
