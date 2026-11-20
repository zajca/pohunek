---
kind: added
---
`pohunek-plugin-kit smoke` takes `--script-timeout <seconds>` (integer
1..=900, absent keeps the 60 s default) bounding only the smoke script; a
script that outlives its bound fails the smoke with an error naming the
script and the bound, and the passing `--json` report carries the effective
bound as `script_timeout_s`. The reusable `plugin-ci.yml` passes its optional
`smoke-script-timeout` input through the same way. ([#2446](https://github.com/zajca/pohunek/issues/2446))
