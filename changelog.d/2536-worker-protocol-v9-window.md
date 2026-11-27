---
kind: changed
breaking: true
---
The private worker protocol window moves to 8..=9 (`PREVIOUS_VERSION` is now 8, `CURRENT_VERSION` is now 9; protocol 9 adds no wire change of its own and opens the window for the offset-based reattach). A session still running on a worker started by a 0.44-era daemon (worker protocol 7) is outside the window: the daemon does not adopt it, `pohunek service check` reports its session as `would_not_be_adopted` with `worker_protocol_incompatible`, and the session becomes unreachable after the upgrade until it is relaunched. ([#2536](https://github.com/zajca/pohunek/issues/2536))
