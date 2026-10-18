## Unreleased (2026-10-18, Linux empty-environ exec window never reads as unmarked, #1176)

- The Linux process reader no longer decides a zero-byte
  `/proc/<pid>/environ` read on its own. The kernel returns end-of-file both
  for a task without a memory map and while a freshly exec'd image has not
  published its environment region yet, so an agent seen inside that exec
  window previously read as a process with no ownership markers, and the
  runtime sweep treated a live worker child as foreign. A lost runtime's
  cleanup could therefore be confirmed while the child survived.
- An empty environ read is now decided by the `/proc/<pid>/stat`
  environment-region bounds (fields 50 and 51, `env_start`/`env_end`): an
  ended or vanished task reads as unmarked as before, a published zero-length
  region on a live task is a genuinely empty environment and reads as
  unmarked, and every other live task — an unpublished region, a reversed or
  half-published one, or a published nonempty region that contradicts the
  empty read — is `Unobservable`, never unmarked. A contradictory pair is
  reread a bounded number of times, which usually recovers the markers;
  exhaustion stays unobservable. The runtime sweep classifies such a process
  as unreadable, so cleanup stays unconfirmed until the markers say
  otherwise.
- Regression scenarios in `pohunek-platform` drive the production reader and
  the actual sweep selection together against controlled procfs observations
  (unpublished, contradictory, recovered, genuinely empty, ended and
  vanished), so a future change cannot reintroduce the window.
