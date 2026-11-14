---
kind: added
---
A `conflict` runtime on `session.list`, `session.inspect` and session events reports `generation_recorded`: `true` when its record names the worker generation (the supervised job) that `session.stop` and `session.remove` prove before acting, `false` when the record names no generation. Peers that predate the field decode without it and are treated as recorded. ([#2145](https://github.com/zajca/pohunek/issues/2145))
