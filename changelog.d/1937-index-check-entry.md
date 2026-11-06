---
kind: added
---
`cargo xtask index check-entry` checks one added plugin index entry against the admitted entries and the pinned core: the package id must be `pohunek.runtime.` plus the runtime id, the runtime id must not be one of the refused ids, the version must be a release above every admitted version, and the core range must admit the pinned core with a lower bound of at least 0.40.0. A refusal names its predicate. The index repository's admission workflow runs it.
