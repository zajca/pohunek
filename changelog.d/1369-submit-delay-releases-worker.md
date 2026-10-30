---
kind: fixed
---
`session input` on a delayed-submit agent profile (such as Claude) no longer blocks other requests to the session during the submit delay. A resize, a stop or an observation request is served while the delay runs, and attach keystrokes are no longer queued behind it. The text is still typed first and Enter follows after at least the configured delay.
