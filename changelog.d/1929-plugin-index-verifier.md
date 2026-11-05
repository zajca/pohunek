---
kind: added
---
The package crate verifies a signed plugin index: a separate document family from the runtime catalog, endorsed directly by one designated offline root, with embedded key revocation, a sequence at most one day ahead of the clock and a lifetime of at most 14 days. Nothing installs from an index yet; the daemon and CLI wiring follow. ([#1929](https://github.com/zajca/pohunek/issues/1929))
