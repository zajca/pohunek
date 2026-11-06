---
kind: fixed
---
A session whose worker stopped answering `session screen`, `session input` and `session stop` after an output burst and a large input is no longer wedged. The daemon now reads the worker connection continuously, closes a connection whose request or answer does not finish (new optional `service.toml` keys `worker_send_ms` and `worker_answer_ms`), and `session stop` ends a worker that does not answer through its service manager.
