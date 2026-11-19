---
kind: fixed
---
`cargo xtask claude canary capture` takes `--first-screen-timeout-secs`, the bound of the wait for the first screen of a session (the step timeout when absent). The flag bounds only that setup wait, so a loaded runner no longer stops the run at an earlier screen than the step the scenario names. The record and the first notification waits keep `--step-timeout-secs`.
([#2431](https://github.com/zajca/pohunek/issues/2431))
