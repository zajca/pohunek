---
kind: fixed
---
A restore that cannot read the host boot identity no longer judges the
recorded worker stale: the session stays unverified and an outside caller is
refused until the host names its boot again, instead of passing the outside
caller off as the owner. A record naming a worker of another boot is still
judged stale, but only in memory; a second restart names the session
unverified again until the runtime is settled.
([#2377](https://github.com/zajca/pohunek/issues/2377))
