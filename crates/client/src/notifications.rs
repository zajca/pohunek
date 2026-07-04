//! Typed SDK helpers for the `notification.*` control methods.
//!
//! These are thin, typed wrappers over [`Client::request`] that serialize the
//! protocol parameter structs and decode the typed result structs. They keep the
//! notification surface discoverable on the SDK [`Client`] itself rather than
//! forcing callers to hand-build [`Request`] envelopes.

use protocol::method;
use protocol::{
    NotificationCreateParams, NotificationCreateResult, NotificationDeleteParams,
    NotificationDeleteResult, NotificationListParams, NotificationListResult,
    NotificationPolicyParams, NotificationPolicyResult, NotificationRetentionParams,
    NotificationRetentionResult, NotificationUpdateParams, NotificationUpdateResult, Request,
};
use serde_json::Value;

use crate::{Client, ClientError};

// Correlation ids for the notification SDK request/response exchanges.
//
// Each helper performs exactly one synchronous exchange whose reply must echo
// this id (verified in `Conn::exchange`), so a stable per-method id is
// sufficient and needs no per-call uniqueness. Distinct per-method ids keep the
// daemon-side request log readable.
const CREATE_REQUEST_ID: &str = "sdk-notification-create";
const LIST_REQUEST_ID: &str = "sdk-notification-list";
const UPDATE_REQUEST_ID: &str = "sdk-notification-update";
const DELETE_REQUEST_ID: &str = "sdk-notification-delete";
const POLICY_GET_REQUEST_ID: &str = "sdk-notification-policy-get";
const POLICY_SET_REQUEST_ID: &str = "sdk-notification-policy-set";
const RETENTION_PRUNE_REQUEST_ID: &str = "sdk-notification-retention-prune";

impl Client {
    /// Create a durable notification record on the connected host.
    pub async fn create_notification(
        &mut self,
        params: NotificationCreateParams,
    ) -> Result<NotificationCreateResult, ClientError> {
        let request = Request::new(
            CREATE_REQUEST_ID,
            method::NOTIFICATION_CREATE,
            serde_json::to_value(params)?,
        );
        let value = self.request(&request).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// List durable notification records matching `params`.
    pub async fn list_notifications(
        &mut self,
        params: NotificationListParams,
    ) -> Result<NotificationListResult, ClientError> {
        let request = Request::new(
            LIST_REQUEST_ID,
            method::NOTIFICATION_LIST,
            serde_json::to_value(params)?,
        );
        let value = self.request(&request).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Update one notification's lifecycle status.
    pub async fn update_notification(
        &mut self,
        params: NotificationUpdateParams,
    ) -> Result<NotificationUpdateResult, ClientError> {
        let request = Request::new(
            UPDATE_REQUEST_ID,
            method::NOTIFICATION_UPDATE,
            serde_json::to_value(params)?,
        );
        let value = self.request(&request).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Delete one notification record.
    pub async fn delete_notification(
        &mut self,
        params: NotificationDeleteParams,
    ) -> Result<NotificationDeleteResult, ClientError> {
        let request = Request::new(
            DELETE_REQUEST_ID,
            method::NOTIFICATION_DELETE,
            serde_json::to_value(params)?,
        );
        let value = self.request(&request).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Read the current notification policy.
    pub async fn get_notification_policy(
        &mut self,
    ) -> Result<NotificationPolicyResult, ClientError> {
        let request = Request::new(
            POLICY_GET_REQUEST_ID,
            method::NOTIFICATION_POLICY_GET,
            Value::Null,
        );
        let value = self.request(&request).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Replace the notification policy.
    pub async fn set_notification_policy(
        &mut self,
        params: NotificationPolicyParams,
    ) -> Result<NotificationPolicyResult, ClientError> {
        let request = Request::new(
            POLICY_SET_REQUEST_ID,
            method::NOTIFICATION_POLICY_SET,
            serde_json::to_value(params)?,
        );
        let value = self.request(&request).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Prune notification records through the retention policy.
    pub async fn prune_notifications(
        &mut self,
        params: NotificationRetentionParams,
    ) -> Result<NotificationRetentionResult, ClientError> {
        let request = Request::new(
            RETENTION_PRUNE_REQUEST_ID,
            method::NOTIFICATION_RETENTION_PRUNE,
            serde_json::to_value(params)?,
        );
        let value = self.request(&request).await?;
        Ok(serde_json::from_value(value)?)
    }
}
