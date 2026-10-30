## Unreleased (2026-10-30, daemon start verifies the active version, #1360)

- `pohunek daemon start` in service mode verifies the active version directory (`layout::verify_installed`) before it runs that version's `pohunekd`, and refuses an untrusted one with `service_version_untrusted`, the code the service engine uses. `--dev-subprocess` is unchanged.
