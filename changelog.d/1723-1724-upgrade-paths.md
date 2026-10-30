---
kind: fixed
---
A session whose worker died before a host reboot now ends with its lost runtime instead of staying `running`: new worker journals record the boot they ran in (`boot_session_id`), and a daemon on another boot treats the loss as proven. `pohunek service upgrade` on a host without Hermes now reports "Hermes is not installed, skipped" instead of "Hermes target is unsafe".
