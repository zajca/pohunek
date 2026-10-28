## Unreleased (2026-10-28, tolerant notification, project, assistant and quarantine enums, #1339)

- A client of an older build reads a notification severity or status, a project source, an action provider, a prompt layer, a concept type and a quarantine reason that a newer daemon adds as `unknown`, and drops an unknown concept intent. `notifications watch` keeps streaming past such a record, and an `unknown` status matches no watch filter. The daemon's params, notification store, project store and governance record stay strict.
