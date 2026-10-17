## Unreleased (2026-10-17, CI test jobs install the Rust toolchain before their tests, #1256)

- The fast test shards and the relay database job install the pinned Rust
  toolchain before they run tests from the nextest archive. A test that runs
  `cargo` no longer makes rustup install the toolchain while tests run, which
  failed tests intermittently when several of them started the install at once.
