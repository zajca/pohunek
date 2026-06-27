# Rust SDK Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the existing CLI control-protocol client into a public Rust SDK crate at `crates/client`, then make the CLI consume that SDK without changing command behavior.

**Architecture:** The new `pohunek-client` crate owns daemon transport concerns: local Unix socket dialing, NetBird TCP dialing, newline-delimited request/response framing, subscription streams, and raw attach stream dialing. The CLI keeps argument parsing, terminal bridging, rendering, and CLI-only validation errors, while converting SDK errors into its existing structured CLI error envelope.

**Tech Stack:** Rust 2021, Tokio, `tokio-util` `LinesCodec`, `futures`, `serde_json`, `thiserror`, existing `pohunek-protocol` and `pohunek-netbird` crates.

---

## Task 1: Workspace and SDK Skeleton

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/client/Cargo.toml`
- Create: `crates/client/src/lib.rs`

- [ ] Add `crates/client` to workspace members.
- [ ] Add `pohunek-client = { path = "crates/client" }` to workspace dependencies.
- [ ] Create the `pohunek-client` package with dependencies on `pohunek-protocol`, `pohunek-netbird`, `serde_json`, `thiserror`, `tokio`, `tokio-util`, and `futures`.
- [ ] Add a minimal `lib.rs` that forbids unsafe code and exports placeholder modules only after failing tests are written.
- [ ] Run `rtk cargo test -p pohunek-client` and expect compilation to fail until tests and implementation are added.

## Task 2: SDK Error Contract

**Files:**
- Create: `crates/client/src/error.rs`
- Modify: `crates/client/src/lib.rs`
- Modify: `crates/cli/src/error.rs`

- [ ] Write SDK unit tests proving local daemon-unreachable, local framing, remote discovery, remote transport, remote daemon-unavailable, and remote protocol errors map to stable `ProtocolError` values.
- [ ] Verify the new tests fail because `ClientError` does not exist.
- [ ] Implement `ClientError` in the SDK with public variants for daemon unreachable, framing, daemon protocol, NetBird resolution, host unreachable, remote daemon unavailable, remote protocol, IO, and JSON failures.
- [ ] Implement `ClientError::to_protocol_error()` so callers can render a stable structured envelope.
- [ ] Keep CLI-only validation errors in `CliError`; add `CliError::Client(#[from] client::ClientError)` and delegate to `ClientError::to_protocol_error()`.
- [ ] Run `rtk cargo test -p pohunek-client error` and the relevant CLI error tests.

## Task 3: Framed Request/Response Client

**Files:**
- Create: `crates/client/src/transport.rs`
- Modify: `crates/client/src/lib.rs`
- Modify: `crates/cli/src/client.rs`

- [ ] Write SDK async tests with in-process Unix/TCP listeners proving `Client::connect_local`, `Client::connect_tcp_addr`, and `Client::request` send a `Request` line and return the OK payload.
- [ ] Write SDK async tests proving daemon-returned `ProtocolError`, garbled replies, closed replies, and oversized lines map to the same error classes/codes as the former CLI client.
- [ ] Verify the tests fail before implementing the transport.
- [ ] Move the generic `Conn<S>` request/exchange logic into the SDK.
- [ ] Expose `Client::connect(host, socket_path)`, `Client::connect_local(socket_path)`, `Client::connect_tcp_addr(host, addr)`, and `Client::request(&Request)`.
- [ ] Keep NetBird host resolution inside the SDK for `Client::connect`.
- [ ] Replace `crates/cli/src/client.rs` with a small compatibility wrapper or re-export so existing command code compiles with minimal call-site churn.
- [ ] Run `rtk cargo test -p pohunek-client transport`.

## Task 4: Subscription and Raw Attach Stream

**Files:**
- Modify: `crates/client/src/transport.rs`
- Modify: `crates/client/src/lib.rs`
- Modify: `crates/cli/src/commands/attach.rs`
- Modify: `crates/cli/src/lib.rs`

- [ ] Write SDK async tests proving `Client::subscribe` verifies the ack and yields subsequent raw event JSON lines without printing.
- [ ] Write SDK async tests proving `connect_raw`, `connect_raw_local`, and `connect_raw_tcp_addr` open unframed streams that can carry an attach header and arbitrary bytes.
- [ ] Verify the tests fail before implementing subscription/raw APIs.
- [ ] Implement a public `Subscription` stream wrapper or equivalent method that lets the CLI print lines while the SDK owns framing.
- [ ] Implement a public `RawStream` enum plus raw connect functions in the SDK.
- [ ] Update CLI `subscribe` command handling to print SDK subscription lines.
- [ ] Update CLI attach code to use SDK `RawStream` and `Client`.
- [ ] Run `rtk cargo test -p pohunek-client subscription raw`.

## Task 5: CLI Integration and Verification

**Files:**
- Modify: `crates/cli/Cargo.toml`
- Modify: all CLI modules importing `crate::client::Client`
- Delete or reduce: `crates/cli/src/client.rs` only if no CLI-specific logic remains.

- [ ] Add `pohunek-client` as a CLI dependency.
- [ ] Update imports to use `client::{Client, RawStream, connect_raw}` through the wrapper or directly.
- [ ] Ensure no CLI command builds its own transport logic.
- [ ] Run `rtk cargo fmt --all`.
- [ ] Run `rtk cargo test -p pohunek-client`.
- [ ] Run `rtk cargo test -p pohunek-cli`.
- [ ] Run `rtk cargo test --workspace`.
- [ ] Run `rtk cargo clippy --all-targets -- -D warnings`.
- [ ] Inspect `rtk git diff --stat` and `rtk git diff` for accidental scope creep.
