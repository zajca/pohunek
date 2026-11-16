---
kind: fixed
---
A profile whose `program` is a version-manager shim (a symlink to the `mise`
manager under the agent's name, as on Omarchy) is probed through the installed
executable the manager names and launched through the shim link. A shim with no
installed release refuses the launch with `agent_runtime_unsupported` and a hint
to run the agent once in a login shell; the version check never installs or
updates a tool. ([#2383](https://github.com/zajca/pohunek/issues/2383))
