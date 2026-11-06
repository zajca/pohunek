---
kind: added
---
`cargo xtask index` gains the tooling that produces a signed plugin index without any key reaching the process: `build` turns an entries tree (`official/<package_id>/<version>.toml`, with a validated `[source]` table that stays out of the document) into an unsigned index, `message` writes the bytes an external signer signs, `attach` inserts that signature after the daemon's own verifier accepted the result, and `verify` checks a signed index, or a key record and a revocation list, with the same rules. `throwaway` creates a self-contained trust root and a signed index for proofs. The checks live in the package crate, so the tool and the daemon cannot disagree. ([#1935](https://github.com/zajca/pohunek/issues/1935))
