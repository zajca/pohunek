---
kind: fixed
---
The daemon removes the private `pohunek-trust-*` repositories that a trusted-checkout evaluation left below the worker state root when the daemon stopped while the evaluation was blocked in a stalled file system call; before, the fetched remote trees stayed on disk across restarts. An evaluation that records its answer just as the launch's 30 second deadline passes now keeps that answer instead of leaving the launch untrusted without a log line.
