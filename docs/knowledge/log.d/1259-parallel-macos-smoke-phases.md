## Unreleased (2026-10-18, parallel macOS archive smoke phases, #1259)

The macOS synthetic-archive scenarios run in a separate job from the real
archive smoke. CI starts the scenarios alongside the rehearsal build, before
bundle assembly. The evidence verdict requires their result and the real
archive smoke; nightly evidence lists both jobs. Isolation remains mandatory.
A rehearsal now starts four macOS jobs instead of three. Rows retain their
producer barrier because their inputs cross the reusable build workflow boundary.
