//! Typed SDK helpers for the `notification.*` control methods.
//!
//! These wrappers keep the notification surface discoverable on [`Client`]
//! without forcing callers to hand-build protocol envelopes.

use protocol::{
    NotificationCreateParams, NotificationCreateResult, NotificationDeleteParams,
    NotificationDeleteResult, NotificationListParams, NotificationListResult,
    NotificationPolicyParams, NotificationPolicyResult, NotificationRetentionParams,
    NotificationRetentionResult, NotificationUpdateParams, NotificationUpdateResult,
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

    /// Update one notification's lifecycle status.
    pub async fn update_notification(
        &mut self,
        params: NotificationUpdateParams,
    ) -> Result<NotificationUpdateResult, ClientError> {
        self.call::<protocol::method::NotificationUpdate>(params)
            .await
    }

    /// Delete one notification record.
    pub async fn delete_notification(
        &mut self,
        params: NotificationDeleteParams,
    ) -> Result<NotificationDeleteResult, ClientError> {
        self.call::<protocol::method::NotificationDelete>(params)
            .await
    }

    /// Read the current notification policy.
    pub async fn get_notification_policy(
        &mut self,
    ) -> Result<NotificationPolicyResult, ClientError> {
        self.call::<protocol::method::NotificationPolicyGet>(())
            .await
    }

    /// Replace the notification policy.
    pub async fn set_notification_policy(
        &mut self,
        params: NotificationPolicyParams,
    ) -> Result<NotificationPolicyResult, ClientError> {
        self.call::<protocol::method::NotificationPolicySet>(params)
            .await
    }

    /// Prune notification records through the retention policy.
    pub async fn prune_notifications(
        &mut self,
        params: NotificationRetentionParams,
    ) -> Result<NotificationRetentionResult, ClientError> {
        self.call::<protocol::method::NotificationRetentionPrune>(params)
            .await
    }
}
