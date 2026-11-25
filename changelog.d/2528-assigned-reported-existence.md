---
kind: fixed
---
Resume of a runtime with `[native_reference] strategy = "assigned"` applies the declared `[native_reference.existence]` check to an id a hook reported, not only to the id the daemon assigned. A reported conversation whose file was moved or deleted is refused with `agent_native_reference_missing` instead of starting an empty conversation under the old id. Stored Pi bindings now go through this check too, because a store from before reference provenance counts its references as reported. The upgrade preflight judges such a session with a missing file `already_not_resumable` and needs no `--accept-runtime-loss`, since the running release only starts an empty conversation under it. ([#2528](https://github.com/zajca/pohunek/issues/2528))
