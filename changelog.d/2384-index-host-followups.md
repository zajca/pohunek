---
kind: added
---
`package_runtime_conflict` now names the holder: `details.holder` carries the
installed package id that serves the runtime id, and is absent (with no
`details` key at all) when a built-in runtime serves it. The CLI prints the
holder in the human error.
([#2384](https://github.com/zajca/pohunek/issues/2384))
