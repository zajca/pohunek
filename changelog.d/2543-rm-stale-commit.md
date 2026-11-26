---
kind: fixed
---
`pohunek session rm` removes a crashed (`failed`) session whose stored record keeps two different runtime instance ids; before, it failed with `session_runtime_commit_stale` on every attempt. ([#2543](https://github.com/zajca/pohunek/issues/2543))
