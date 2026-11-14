---
kind: fixed
---
An approval that an agent's OSC 7501 `blocked` record raised no longer turns `acknowledged` when the agent only redraws its screen after the record was dropped (a root `clear`, a lag resync, a reconfigure): such output is a repaint, so the record stays unread until the agent's next working record or working screen rule, whether the terminal delivered the redraw in one chunk or several. ([#2339](https://github.com/zajca/pohunek/issues/2339))
