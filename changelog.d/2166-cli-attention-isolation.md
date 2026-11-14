---
kind: added
---
`pohunek session new --meta pohunek.attention.children=isolate` sets the reserved pair on the new session, so attention of everything spawned under it stays local to the subtree (any other value for the key is `bad_request`). A host whose daemon does not advertise `session_attention_isolation_supported` is refused with `session_attention_isolation_unsupported` before the session is created. Isolated descendants read `isolated <id>: attention isolated by <isolator>` under the `session list` table, and `--json` carries `attention_isolated_by` on the row only. ([#2166](https://github.com/zajca/pohunek/issues/2166))
