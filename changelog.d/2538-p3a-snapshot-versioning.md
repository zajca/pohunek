---
kind: added
---
`session.screen` results can now carry two optional fields, `screen_model` (`"vt100"` or `"libghostty"`) and `format_version` (a number, currently `1`), naming the screen model that rendered the snapshot and the wire format version of the payload. Both are omitted by a worker or daemon that predates them and read as `vt100` / `1` by their absence, so older clients and daemons keep working unchanged; no protocol version bump was needed. The default screen model is still vt100 in this release — the libghostty switch lands separately. ([#2538](https://github.com/zajca/pohunek/issues/2538))
