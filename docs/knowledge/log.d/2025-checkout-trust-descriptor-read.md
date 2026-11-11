## Unreleased (2026-11-11, Checkout trust descriptor-based read, #2025)

- The trusted-checkout check opens every component below the launch directory relative to its parent without following a symbolic link, reads each declared file up to 4 MiB (64 MiB in total) in-process, and hashes all files with one git process. Updated [runtime-packages](guides/runtime-packages.md).
