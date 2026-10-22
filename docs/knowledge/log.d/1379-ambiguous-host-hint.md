## Unreleased (2026-10-22, ambiguous host hint, #1379)

The `overlay_host_ambiguous` hint offers a per-overlay `fqdn~` selector when only
another overlay carries the same name, and a provider collision that lists no
candidates reads `matches multiple hosts` instead of a count of one. See
[remote hosts](guides/remote-hosts.md).
