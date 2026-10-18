## Unreleased (2026-10-18, subprocess worker descriptor isolation, #302)

The development/test subprocess worker launcher preserves only its explicit
standard streams across exec. Inheritable harness descriptors are marked
close-on-exec before the real worker starts, using Linux close_range with a
bounded fallback on older kernels and macOS. The parent captures both the
soft descriptor limit and existing descriptors above a lowered limit. The real-daemon
startup scenario checks an inheritable sentinel is present in the daemon but
absent from its live worker, while retaining session removal and worker cleanup.
