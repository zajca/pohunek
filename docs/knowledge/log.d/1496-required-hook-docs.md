## Unreleased (2026-10-22, required hook documentation, #1496)

The `service.toml` schema paragraph of `docs/public-api.md` now lists the
optional `[hooks] required` key (additive, omitted while empty, ignored by a
previous release), and the stderr-tail section of the hooks design note names
the `hook_required_failed` error `msg` as the second place the `session.new`
response carries the tail.
