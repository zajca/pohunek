---
kind: fixed
---
On macOS the live transcript watcher keeps its `FSEvents` stream across reconciliation passes instead of recreating it every pass, so a transcript change made right after a pass is indexed by a live event and no longer waits up to 30 s for the next pass. The stream is recreated only when the set of transcript roots or a root's canonical path changes, after the event queue overflowed, and by a full rebuild every 10 minutes that heals a stream that never started or went silent; until then a change that is not delivered live is still indexed by the next 30 s pass. Linux behaviour is unchanged.
