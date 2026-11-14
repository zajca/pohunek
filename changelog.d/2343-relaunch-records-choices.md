---
kind: fixed
---
A session resumed with `--accept-profile-change` after its profile gained an option now records the choice the relaunch took (the new option's default) in `agent_options`, so `session list` and `session inspect` describe the running launch and a later change of that default resumes without another decision. ([#2343](https://github.com/zajca/pohunek/issues/2343))
