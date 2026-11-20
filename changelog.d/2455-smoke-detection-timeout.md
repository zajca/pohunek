---
kind: added
---
`pohunek-plugin-kit smoke` takes `--detection-timeout <seconds>` (integer
1..=60, absent keeps the 60 s default) bounding the wait for the detector to
leave `unknown`. The option can only tighten the bound: a larger value, or
any value for a commands-only package, is refused before the rig starts. A
runtime that stays `unknown` past the bound fails the smoke with an error
naming the bound, and the passing `--json` report carries the effective
bound as `detection_timeout_s`. ([#2455](https://github.com/zajca/pohunek/issues/2455))
