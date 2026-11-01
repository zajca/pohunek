---
kind: changed
---
The per-kind notification policy is a map from kind to flag with the same JSON, so a client of this release keeps a kind a newer daemon adds, lists a notification of such a kind as `unknown` and sends the kind back on `notifications policy set`. The daemon still refuses an unknown kind in params and in `policy.json`, and a kind the policy leaves out takes the daemon's default, so `notification.policy.get` and `notifications policy get` return a flag for every kind. The generated TypeScript `NotificationKindPolicy` has an optional key per kind.
