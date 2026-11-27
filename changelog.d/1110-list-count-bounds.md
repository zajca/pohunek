---
kind: changed
---
`project.actions` and the daemon's `host.discover` reply are hard-bounded at 3200 actions and 1500 peer records respectively and checked against the control-line limit: above either bound the daemon answers the typed `project_actions_too_large` / `host_discover_too_large` error instead of a frame the client refuses. Action and template names are capped at 128 bytes (`invalid_name` above that), and `host.discover` peer records cap their supplied fields (`name`, `fqdn`, `peer_id`, `daemon_version`) at build time, reporting an over-cap field through the record's bounded `reason` instead of echoing it. `package.list` documents its existing `MAX_PACKAGES` (1024) registry bound. ([#1110](https://github.com/zajca/pohunek/issues/1110))
