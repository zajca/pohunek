## Unreleased (2026-10-30, delayed submit releases the worker, #1369)

- A delayed `session.input` submit is two worker plans (body, then Enter) with the submit delay waited in the daemon, for waited and plain input alike. The worker control connection is free during the delay, so resize, stop and observation requests are served meanwhile, and attach input may land between the two plans; the input gate still keeps other input to the session out. The worker holds its PTY lock per fragment write and releases it across a plan's `delay_after`, while control plans never interleave with each other.
