---
kind: added
---
`cargo xtask plugin features-check` (run by the `docs` job of CI) is the feature-removal check of #1659 D4: it fails when a runtime feature required by a package is missing from the registry. It collects `requires` from `runtime-packages/*/runtime.toml` and from every `runtime` entry of an index document (the entry's archive is fetched from its `url`, verified against the entry's digest and read for its descriptor; a context entry is skipped). Without `--index <file>` the official index v2 is fetched, and a failed fetch prints a `warn` naming the URL while the check continues with the other sources, so it never fails on the network alone. ([#1659](https://github.com/zajca/pohunek/issues/1659))
