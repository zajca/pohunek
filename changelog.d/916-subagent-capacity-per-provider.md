---
kind: fixed
---
The limit of 64 running subagents is now counted per provider. Codex children whose interrupted turn or close fires no stop stay `running` until the runtime ends, and they no longer make the worker refuse Claude or Muse subagent starts.
