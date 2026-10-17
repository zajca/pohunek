## Unreleased (2026-10-17, pinned tool downloads back off between retries, #1248)

- `setup-mold`, `setup-sccache` and `setup-zig` no longer pass `--retry-delay`
  to curl, so curl backs off exponentially between retries (1, 2, 4 ... s).
  Six retries span 63 s, so a short outage of the release CDN (504) no longer
  fails a required job. A fixed 2 s delay spent all retries in about 10 s.
