## Unreleased (2026-10-30, boot proof for lost runtimes and Hermes skip, #1723 #1724)

- A worker journals `boot_session_id` (boot session UUID on macOS, kernel boot id on Linux; additive, no schema bump). A daemon whose host names another boot treats the lost runtime as proven ended without sweeping, so the session no longer stays `running` after a reboot. Journals of earlier workers lack the field and keep the unconfirmed-cleanup outcome.
- The upgrade's integration reinstall reports a host without `~/.hermes` as skipped (`gap`, "Hermes is not installed ... skipped") instead of resolving the missing path into "Hermes target is unsafe".
