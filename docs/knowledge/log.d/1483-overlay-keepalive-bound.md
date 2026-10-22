## Unreleased (2026-10-22, overlay keepalive release bound, #1483)

The sessions concept page states the release bound of a waiter slot held by an overlay (TCP) client that vanishes without closing its connection: TCP keepalive releases it within `idle + interval x count` (60 s by default), and the wait's timeout stays the bound only for a client that pipelines another request behind the wait. See [sessions](concepts/sessions.md).
