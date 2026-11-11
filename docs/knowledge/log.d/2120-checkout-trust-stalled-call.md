## Unreleased (2026-11-11, Checkout trust stalled file system call, #2120)

- A launch stops waiting for a trusted-checkout evaluation that has not finished within its 30 second deadline, for example one stalled in a file system call on a hung mount below a declared path. It withholds the trusted arguments, logs one `checkout_trust.failed` warning and no longer holds the session launch for the stalled call. Updated [runtime-packages](guides/runtime-packages.md).
