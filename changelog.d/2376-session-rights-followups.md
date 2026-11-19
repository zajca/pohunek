---
kind: security
---
A `session.retention.sweep` from inside a session now sweeps only that session's own subtree: a record outside it survives even when the policy selects it, and a `session_ids` entry outside it is not examined and not reported, like an unknown id. Connections the daemon cannot place are refused the sweep outright, including `dry_run`. `plugin profile fold` treats `[rights]` like any other launch key: inputs whose tables differ no longer fold into one parent (a preset takes its parent's rights), and an equal table is carried to the proposed parent. ([#2376](https://github.com/zajca/pohunek/issues/2376))
