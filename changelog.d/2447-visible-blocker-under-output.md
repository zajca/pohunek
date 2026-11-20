---
kind: fixed
---
A blocking dialog a screen rule marks `visible_blocker` stays `blocked` while
the agent keeps writing output that changes no screen text (cursor moves,
cursor show/hide, a redraw of the same frame); it no longer reads `working`
over the dialog for programs that report no program-status record, such as
Mistral Vibe. A `visible_blocker` title rule that outranks the dialog (Codex
"Action Required") holds `blocked` the same way while the screen still shows
the dialog, with source `osc_title`. ([#2447](https://github.com/zajca/pohunek/issues/2447))
