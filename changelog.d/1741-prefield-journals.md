---
kind: fixed
---
On Linux a session whose worker died before a reboot now ends with its lost runtime even when the worker journal predates `boot_session_id`: the journal's own `boot_identity` is the kernel boot id and proves the reboot. `pohunek session rm --accept-unconfirmed-cleanup` now accepts up to 1024 unreadable processes instead of 64, never signals them, lists the first 64 in the result and gives the full count in the new optional `accepted_unconfirmed_total`.
