---
kind: fixed
---
The daemon now tracks its control and remote connections and stops them when it is stopped. An idle connection and a pipelined request line are answered with `daemon_shutting_down` and the connection closes; an event subscription and an attach stream end cleanly, and a mutating request in flight finishes and is answered first. Previously the connection tasks were not tracked, so a client saw the connection drop when the runtime was dropped, and a request could start after the stop signal. A connection that outlives the shutdown bound is named as `control_connections` or `remote_connections`.
