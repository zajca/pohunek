## Unreleased (2026-11-11, checkout trust scratch sweep, #2141)

- Daemon start removes every `pohunek-trust-*` scratch repository below `<worker state root>/checkout-trust` (the daemon is its only writer, so each one is left over from a daemon that stopped during a stalled evaluation); other entries are kept, and a removal failure logs `checkout_trust.sweep_failed` without stopping the daemon. An evaluation that recorded its answer when the launch's deadline passed keeps that answer. See [Runtime packages](guides/runtime-packages.md).
