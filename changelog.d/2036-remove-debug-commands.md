---
kind: removed
breaking: true
---
`pohunek store rejected`, `pohunek session runtime-inventory` and `pohunek session policy` are removed together with the `store.rejected_list`, `store.rejected_set_aside`, `session.runtime_inventory`, `session.policy.get` and `session.policy.set` methods; the daemon answers them with `method_not_found` on every protocol version, including protocol 3. The session retention policy keeps its stored values and the `session retention sweep` command still applies it; `pohunek doctor` still reports metadata-store lines no loader accepts, and `session detection` is unchanged.
