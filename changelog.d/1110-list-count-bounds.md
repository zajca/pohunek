---
kind: changed
---
`project.actions` and the daemon's `host.discover` reply are hard-bounded at 1024 records each and checked against the control-line limit: above either bound the daemon answers the typed `project_actions_too_large` / `host_discover_too_large` error instead of a frame the client refuses. `package.list` documents its existing `MAX_PACKAGES` (1024) registry bound. ([#1110](https://github.com/zajca/pohunek/issues/1110))
