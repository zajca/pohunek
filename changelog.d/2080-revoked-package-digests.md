---
kind: added
---
A runtime package whose digest a later signed catalog revokes is now refused. The revoked digests are kept in a new `revoked-digests.json` beside `catalog-state.json`, which keeps its schema so a rollback still reads the registry. `session new`, resume, `plugin select`, enabling, profile pins and `integration install --agent hermes` fail with `package_revoked` and name `pohunek plugin select`; the selection is never moved on its own. Sessions already running from the digest keep running and are flagged in `session list` (warning `package_revoked`) and `plugin doctor` (finding `revoked`).
