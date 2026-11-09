//! Typed SDK helpers for the `notification.*` control methods.
//!
//! These wrappers keep the notification surface discoverable on [`Client`]
//! without forcing callers to hand-build protocol envelopes.

use protocol::{
    NotificationCreateParams, NotificationCreateResult, NotificationListParams,
    NotificationListResult,
};

use crate::{Client, ClientError};

impl Client {
    /// Create a durable notification record on the connected host.
    pub async fn create_notification(
        &mut self,
        params: NotificationCreateParams,
    ) -> Result<NotificationCreateResult, ClientError> {
        self.call::<protocol::method::NotificationCreate>(params)
            .await
    }

    /// List durable notification records matching `params`.
    pub async fn list_notifications(
        &mut self,
        params: NotificationListParams,
    ) -> Result<NotificationListResult, ClientError> {
        self.call::<protocol::method::NotificationList>(params)
            .await
    }
}
