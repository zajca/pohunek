## Unreleased (2026-11-16, NetBird non-owner account, #2382)

- `netbird_state_restricted` remedy now names the `[netbird.static_peers.<name>]` table, the host-id source via `pohunek host governance inspect local --json`, and `sudo netbird profile claim` with `netbird profile list --show-owner`. `overlay::OverlayError::StateRestricted` gains a required `remedy: String` field.
- Multi-overlay discovery failure keeps the restricted overlay remedy in `recover` with overlay prefix.
- Restricted error detail distinguishes a missing static-peer table, a NetBird daemon that is not Connected, and an unlisted host; the discovery path notes that static peers are used only by `--host <name>` and never appear in discovery.
- Doctor warning for restricted NetBird now includes the remedy text.
- Docs decision list explains NetBird ownership model, static-peer remedy, `profile claim` cost, `NB_DISABLE_PROFILE_OWNERSHIP` warning, and removal of NetBird downgrade suggestion.
