## Unreleased (2026-10-13, Hermes docs match the package-only runtime, #147)

- The Hermes operator guide (and the operator skill generated from it) states
  what the official package carries, the launch, resume and detection
  contract in `runtime.toml` and `detect.toml`, instead of a built-in contract
  it preserves.
- `docs/features.md`, `docs/README.md` and `docs/ROADMAP.md` describe Codex,
  Claude Code and Hermes as served only by their official runtime packages:
  only `shell` is built in, the Linux x86_64 daemon archives ship the
  packages, macOS daemons ship none until #929, and without its package a bare
  runtime is `agent_profile_not_found` and a host profile over it
  `runtime_not_installed`.
