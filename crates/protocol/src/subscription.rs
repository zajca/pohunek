//! Wire types of the `subscribe` stream's lag and heartbeat signals.
//!
//! A subscriber that reads too slowly loses events; the daemon tells it with a
//! `subscription_lagged` event so it can re-seed from the list methods. A
//! subscriber may also ask for periodic `heartbeat` events while the stream is
//! quiet, so a dead connection shows up without a probe request.

// Rust guideline compliant 2026-11-26

use serde::{Deserialize, Serialize};

/// Optional parameters of the `subscribe` request.
///
/// A request with `null` or `{}` params receives no heartbeat.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SubscribeParams.ts"))]
pub struct SubscribeParams {
    /// Quiet-stream interval, in milliseconds, after which the daemon sends a
    /// `heartbeat` event. Must lie within the bounds the daemon advertises in
    /// [`SubscribeHeartbeatCapability`]; absent means no heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub heartbeat_ms: Option<u32>,
}

/// The `subscribe` heartbeat support a daemon advertises in `host.inspect`.
///
/// A client sends `heartbeat_ms` only to a daemon that carries this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "SubscribeHeartbeatCapability.ts")
)]
pub struct SubscribeHeartbeatCapability {
    /// Smallest accepted `heartbeat_ms`.
    pub min_ms: u32,
    /// Largest accepted `heartbeat_ms`.
    pub max_ms: u32,
}

/// The broadcast channel a subscriber lagged on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", remote = "Self")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SubscriptionChannel.ts"))]
pub enum SubscriptionChannel {
    /// Session lifecycle and agent-state events; re-seed with `session.list`.
    Session,
    /// Notification events; re-seed with `notification.list`.
    Notification,
    /// A channel a newer daemon sent that this build does not know. Only the
    /// client's [`tolerant`](crate::tolerant) decode produces it; the client
    /// re-seeds everything it follows.
    #[doc(hidden)]
    #[serde(rename = "unknown", skip_deserializing)]
    #[cfg_attr(feature = "ts", ts(skip))]
    Unknown,
}

crate::tolerant_enum!(SubscriptionChannel);

/// Payload of a `subscription_lagged` event.
///
/// A lag is per channel, never per connection: the event precedes every later
/// event of `channel` only. The subscriber missed `skipped` events of `channel`
/// and re-seeds from the list method of that channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SubscriptionLaggedEvent.ts"))]
pub struct SubscriptionLaggedEvent {
    /// Channel whose events were dropped.
    pub channel: SubscriptionChannel,
    /// Number of events dropped.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub skipped: u64,
}

/// Payload of a `heartbeat` event: no fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HeartbeatEvent.ts"))]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "an empty named-field struct serializes as the JSON object `{}`, which the wire payload requires"
)]
pub struct HeartbeatEvent {}
