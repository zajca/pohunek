## Unreleased (2026-11-16, the feature-removal check, #1659)

- `cargo xtask plugin features-check [--index <file>]` fails when a runtime
  feature a package requires is missing from the registry (the
  `crates/daemon/src/agent/host/fixtures/features.txt` snapshot the
  append-only guard compares `RUNTIME_FEATURES` against): a removal waits until
  nothing requires the feature. It collects `requires` from
  `runtime-packages/*/runtime.toml` and from every `runtime` entry of an index
  document, whose archive is fetched from the entry's `url`, verified against
  the entry's digest and read for its descriptor (a digest mismatch fails; a
  context entry has no runtime descriptor and is skipped; the index signature
  is not verified, the check reads requirements and authorizes nothing).
  Without `--index` the official index v2 is fetched; a failed fetch (of the
  index or of an entry archive) prints a `warn` naming the URL and the check
  continues with the other sources, so it never fails on the network alone.
  ([Plugin repositories](guides/plugin-repositories.md)).
