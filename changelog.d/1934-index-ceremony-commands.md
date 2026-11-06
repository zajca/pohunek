---
kind: added
---
`cargo xtask` gains the owner ceremony commands of the plugin index: `catalog keygen` creates an owner-private Ed25519 key file, `index endorse` signs the `official` record of an index key, `index revoke` writes the next revocation list (it never shrinks), and `index channel` writes the channel file for a root of the trust anchor. Keys are read only from files and never printed, outputs are created exclusively, and every document is verified with the daemon's rules before it is written. The key ceremony and compromise procedure are in the new plugin index keys runbook. ([#1934](https://github.com/zajca/pohunek/issues/1934))
