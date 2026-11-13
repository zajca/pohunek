---
kind: added
---
A `main` push of the CI workflow can now reuse the green jobs of earlier runs of the same tree (pull-request, push, or tier=main dispatch, within 24 hours) while the `CI_REUSE_PUSH` kill switch is on, so a merge whose pull-request run already tested the identical tree finishes faster; the kill switch is read in a step, never in a job condition, and a failed reuse still fails the verdict.
