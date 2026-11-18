---
kind: added
---
`package_runtime_conflict` now names the holder: `details.holder` carries the
installed package id that serves the runtime id, and is absent when a built-in
runtime serves it. The CLI prints the holder in the human error, and installs
by id refresh a stored index copy that is below the revocation high-water
mark, not only below the index sequence mark.
([#2384](https://github.com/zajca/pohunek/issues/2384))
