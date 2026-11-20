---
kind: added
---
`scripts/guard-pack --base <rev>` runs the pull-request gates a local gate run misses (changelog fragment, package version bump against the last release, macOS test selection for an added `#[ignore`, docs check), prints one line per gate and exits 1 on any failure; a gate that only CI can decide is listed as `UNDECIDED` in the summary line. ([#2179](https://github.com/zajca/pohunek/issues/2179))
