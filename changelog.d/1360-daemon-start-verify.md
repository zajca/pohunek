---
kind: fixed
---
`pohunek daemon start` in service mode now verifies the active version directory the way the service engine does before it runs that version's `pohunekd`, and refuses a directory or binary that another account could have changed with `service_version_untrusted`. A missing active version directory is reported with the same code instead of `service_io_failed`, and so do the service upgrade, rollback and setup checks of an installed version. `pohunek service uninstall` no longer starts a stopped daemon from an active version directory that fails that check, so removing the installation does not run the untrusted `pohunekd`.
