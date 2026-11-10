---
kind: added
---
The daemon writes an audit entry on the `audit.session_change` log target for every request that changes a session (`session.new`, `stop`, `resume`, `remove`, `input`, `detach`, `set_metadata`, `rename`, `retention.sweep`, and opening an attach stream), naming the transport, the peer (the overlay address for a remote connection, the kernel-attested pid and uid for the local socket), the request kind, the session and the outcome. Input text, keys and attach stream ids are never logged.
