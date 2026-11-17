---
kind: fixed
---
`host.inspect` now shares one version-manager lookup per inventory for
profiles behind the same shim and preflight probes carry the owner
locations in their key, so two profiles naming different installs through
different location variables no longer judge one session by the other
profile's release. A launch whose owner location value the daemon cannot
read is refused naming the variable, never its value, and a runtime
without a version probe resolves its program the same way a probed one
does.
([#2387](https://github.com/zajca/pohunek/issues/2387))
