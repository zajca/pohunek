//! Typed SDK helpers for read-only integration status.

// Rust guideline compliant 2026-06-26

use protocol::{IntegrationStatusParams, IntegrationStatusResult};

use crate::{Client, ClientError};

impl Client {
    /// Inspect daemon-managed hook integrations without mutation.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when transport, protocol, or decoding fails.
    pub async fn integration_status(
        &mut self,
        params: IntegrationStatusParams,
    ) -> Result<IntegrationStatusResult, ClientError> {
        self.call::<protocol::method::IntegrationStatus>(params)
            .await
    }
}
