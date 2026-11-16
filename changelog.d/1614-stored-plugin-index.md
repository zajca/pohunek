---
kind: added
---
The host keeps a stored official plugin index. `pohunek plugin index refresh`
fetches and stores it, a 24 hour cadence keeps it current, and installs by id
use a fresh stored copy and refresh when it is expired, below the daemon's
sequence marks, or does not list the package. The daemon verifies and applies
an index through `plugin.index.apply`, refuses a rollback with
`package_index_rollback`, and refuses an unreadable state file with
`package_index_state_unreadable`. `daemon.doctor` reports the stored index in
the `plugin_index` check, and official runtime ids are reserved.
([#1614](https://github.com/zajca/pohunek/issues/1614))
