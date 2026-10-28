---
kind: changed
---
`pohunek notifications watch`, `project` and `host governance inspect` no longer fail on a notification severity or status, project source, action provider, prompt layer or quarantine reason that a newer daemon adds: the value reads as `unknown`, and `notifications watch` keeps streaming.
