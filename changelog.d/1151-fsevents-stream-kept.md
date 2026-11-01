---
kind: fixed
---
On macOS the live transcript watcher keeps its `FSEvents` stream across reconciliation passes instead of recreating it every pass, so a transcript change made right after a pass is indexed by a live event and no longer waits up to 30 s for the next pass. A stream is recreated only when the set of transcript roots or a root's canonical path changes; Linux behaviour is unchanged.
