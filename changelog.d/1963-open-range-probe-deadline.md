---
kind: added
---
A runtime package can declare an open-ended supported range: a `version_probe` with `args` and `min` and no `below` admits every release from `min` on. The descriptor lists the new runtime feature `version-probe-open-range-v1` in `requires`; an older core refuses such a package with `runtime_feature_unsupported`, and a release below `min` is refused with the minimum named. A version probe that runs to its 2 second deadline is now refused as `agent_runtime_unsupported` with its own message and a `recover` hint, and `pohunek doctor` reports it. The probe has always received the runtime's `[launch_env]`; the documentation says so.
