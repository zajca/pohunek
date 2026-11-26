---
kind: fixed
---
`session_cwd_missing` now names the absolute directory that does not exist, and for the assistant directory points to `pohunek assistant`, which creates it. A kept `session new` whose agent waits on a dialog prints where to answer it, and `session_input_not_ready` no longer shows the "raise the grace" hint next to a `blocked` activity. `claude-accounts add <name>` names the account it added instead of the last one in the list, and `claude-accounts add` no longer fails on macOS, where the BSD `chmod` rejected the `--` placed after the mode (Claude package 2.3.1). ([#2508](https://github.com/zajca/pohunek/issues/2508), [#2509](https://github.com/zajca/pohunek/issues/2509), [#2540](https://github.com/zajca/pohunek/issues/2540))
