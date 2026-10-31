---
kind: changed
---
The Linux (glibc) daemon limits its malloc arenas to two at startup, which keeps its memory use near the size of its live data under heavy session churn (about 100 MB instead of 330 MB with 400 live sessions in the soak behind #1672). Set `MALLOC_ARENA_MAX` (or `GLIBC_TUNABLES=glibc.malloc.arena_max`) in the daemon's environment to choose another limit; the daemon then leaves the allocator alone. Other platforms are unchanged.
