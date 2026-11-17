---
kind: fixed
---
`host.inspect` now shares one version-manager lookup per inventory for
profiles behind the same shim and preflight probes carry the owner
locations in their key, so two profiles naming different installs through
different location variables no longer judge one session by the other
profile's release.
([#2387](https://github.com/zajca/pohunek/issues/2387))
