---
kind: added
---
A `workflow_dispatch` run of `ci.yml` with `tier=main` accepts `delta: true`: the run reuses the green jobs of the newest same-tree pull request run (recorded by `scripts/ci-reuse`) instead of running them again, so a main-tier evidence dispatch after a green pull request consumes only the jobs the selection narrowed away. `scripts/ci-evidence` accepts that union as evidence for the commit when the skipped jobs are green and fresh in the recorded source run, and names both runs in its report. Full dispatches stay the default (`delta: false`); `scripts/release` keeps dispatching full runs. ([#2203](https://github.com/zajca/pohunek/issues/2203))
