---
kind: changed
---
`pohunek host list` and `pohunek doctor` now say which `tailscale`, `netbird` or agent executable they found but refused to run, with the path and the reason (for example a directory other accounts can write), instead of reporting it as not installed. The error codes are unchanged. On macOS, a directory dropped from the service `PATH` says whether others or a group other than `admin` can write it.
