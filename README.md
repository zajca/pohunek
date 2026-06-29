# pohunek

`pohunek` is a single-user control plane for durable coding-agent sessions across
your own machines. A Rust daemon owns PTYs and agent processes; the Rust CLI
controls it locally over a Unix socket and remotely over a NetBird/WireGuard
address.

The project is pre-1.0. Wire shapes, config files, and on-disk metadata may
change before a stable SDK contract is published.

## Install

Download a release archive for your target, or build the three binaries from a
checked-out repository:

```bash
cargo build --release --locked --bin pohunek --bin pohunekd --bin pohunek-gui
```

The release archive includes the CLI, daemon, native GUI, root README, MIT
license, and the offline documentation bundle.

## Quick Start

```bash
pohunek doctor --json
pohunek daemon start --detach
pohunek health --json
pohunek session new --agent codex
pohunek session list
pohunek session attach <session-id>
```

Use `pohunek project add`, `pohunek project list`, and `pohunek project actions`
to drive repository-aware launcher flows.

## Trust Boundary

`pohunek` is designed for one operator on machines they control. Local access is
guarded by owner-only Unix socket and state-file permissions. Remote access is
expected to be restricted by the user's NetBird/WireGuard private network.

It is not a multi-user authorization system, hosted control plane, or shared
tenant service.

## Documentation

In release archives, the packaged offline docs live under `docs/offline/`.
In a source checkout, start with:

- [Documentation index](docs/README.md)
- [Architecture](docs/architecture.md)
- [Roadmap](docs/ROADMAP.md)
- [Offline knowledge source](docs/knowledge/)
