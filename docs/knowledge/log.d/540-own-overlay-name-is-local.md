## Unreleased (2026-10-15, own overlay name selects the local daemon, #540)

- `--host <name>` and `<name>/<session-id>` targets resolve this machine's own
  overlay name to the local daemon, like `local`, without dialing the mesh.
  The identity comes from the overlay client's own status (`NetBird` root `fqdn`
  and `netbirdIp`), never from the OS hostname. Exact fqdn, own IP, and short
  name match. A short name shared with a peer is `overlay_host_ambiguous`.
- The CLI resolves the hosts a command selects once, before any host check, so
  confirmations, project path rules, workflow and assistant checks treat the
  own name as local. Those hosts are each target's effective host, a positional
  host (`host inspect`, `host governance inspect`), and `--host` where the
  command reads it; a `--host` the command ignores or an explicit target host
  overrides is never resolved. Literal `local` never runs `netbird`. Each
  distinct selected host other than `local` costs one extra `netbird status`
  read, usually one per command.
- Dynamic completion treats the own name as local too: session targets list
  the local sessions, and workflow lines complete the local store.
- `pohunek host list` and `host discover` show this machine first, routed as
  `local`. The own-identity lookup is bounded by the 5 s connect timeout; past
  it the row is left out and the records print. The `--json` records are
  unchanged.
- The overlay provider contract gains `resolve_selector` and `local_identity`,
  with defaults that keep providers without self-knowledge peer-only.
  `OverlayRegistry::resolve_target` and `local_identities` aggregate them.
  `pohunek_client::names_this_machine` exposes the check. `Client::connect`
  and the TS SDK are unchanged.
