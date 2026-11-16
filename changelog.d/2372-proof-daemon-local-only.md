---
kind: fixed
---
Proof daemons and the release smoke start with `--local-only`, so the
overlay listener stays disabled (`remote control listener disabled by
--local-only` in the daemon log) on the proof host and on the release.
([#2372](https://github.com/zajca/pohunek/issues/2372))
