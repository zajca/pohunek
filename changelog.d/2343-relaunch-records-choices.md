---
kind: fixed
---
A session resumed with `--accept-profile-change` after its profile gained an option now records the choice the relaunch resolved (the new option's default) in `agent_options`, so `session list` and `session inspect` name the choice whose env the relaunch applies and a later change of that default resumes without another decision. The relaunch keeps its frozen arguments: the new choice's arguments apply only to a fresh session. ([#2343](https://github.com/zajca/pohunek/issues/2343))
