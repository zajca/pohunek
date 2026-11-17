---
kind: fixed
---
`cargo xtask plugin prove` refuses a `--pre-install` archive whose package id
differs from the proved entry's (the proof fails at `install`); the inspect
wording now names the digest check as the refusal of an unselected version,
which any other installed version of the same id leaves behind.
`cargo xtask plugin features-check` fails (exit 1) on a `file://` entry
archive over the size cap instead of warning and passing, and the exit-status
docs now state that unreadable local-tree files exit 2.
([#2378](https://github.com/zajca/pohunek/issues/2378),
[#2393](https://github.com/zajca/pohunek/issues/2393))
