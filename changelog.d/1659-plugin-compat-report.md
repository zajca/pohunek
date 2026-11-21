---
kind: added
---
`pohunek plugin compat [--json]` reports what the local daemon's core accepts
from a runtime package: the core version, the protocol version, the
`runtime_api` generation and every runtime feature id a descriptor may list in
`requires`. `host.inspect` carries the same two optional fields,
`runtime_api` and `runtime_features`, also with `skip_runtime_probes`. Against a
daemon that does not report them the command fails with
`plugin_compat_unsupported` instead of printing an empty list.
([#1659](https://github.com/zajca/pohunek/issues/1659))
