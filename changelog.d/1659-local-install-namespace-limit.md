---
kind: changed
---
The runtime packages guide now states what a local install (`--sha256` or
`plugin link`) refuses: reserved runtime ids, not a `pohunek.*` package id. It
names the `origin` field of `plugin list --json` and `plugin inspect --json`
(`explicit_digest`, `link` or `official`) as the way to tell a local package
from an official one.
([#1659](https://github.com/zajca/pohunek/issues/1659))
