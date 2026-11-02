---
kind: changed
---
A refused `tailscale` or `netbird` CLI now gets a recovery hint that points at the ownership and permissions it names instead of at installing the CLI. On macOS, the reason for an `admin`-group directory that the effective user does not own, or that root looks up, now says so.
