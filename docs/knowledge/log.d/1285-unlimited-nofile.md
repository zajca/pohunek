## Unreleased (2026-10-19, unlimited descriptor limits, #1285)

Worker launch accepts an unlimited soft descriptor limit when the platform
provides a finite descriptor maximum. The pre-exec isolation sweep still covers
that maximum and any already-open descriptor above it, and fails closed when
no finite bound can be obtained.
