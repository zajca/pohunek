---
kind: removed
breaking: true
---
The daemon no longer stops sessions for being idle. The `session.idle_stop.get` and `session.idle_stop.set` methods, the `pohunek.idle_stop_after_secs` opt-in, the `session policy --idle-stop-after` option and the `session_idle_stop_supported` capability flag are removed; a session that was opted in keeps running, and the metadata key stays an ordinary key. Manager tooling that needs idle handling keeps its own detection.
