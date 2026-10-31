---
kind: changed
---
The Linux (glibc) daemon limits its malloc arenas to two at startup, which keeps its memory use near the size of its live data under heavy session churn (about 100 MB instead of 330 MB with 400 live sessions in the soak behind #1672, on a host with transparent huge pages set to `always`). Set `MALLOC_ARENA_MAX=<n>` (or `GLIBC_TUNABLES=glibc.malloc.arena_max=<n>`) in the daemon's environment to choose another limit; the daemon then leaves the allocator alone. Other platforms are unchanged.
