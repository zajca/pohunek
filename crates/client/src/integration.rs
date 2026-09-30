//! Typed SDK helpers for integration status, diagnosis, and removal.

// Rust guideline compliant 2026-06-26

use protocol::{
    IntegrationDoctorParams, IntegrationDoctorResult, IntegrationStatusParams,
    IntegrationStatusResult, IntegrationUninstallParams, IntegrationUninstallResult,
};

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

    /// Diagnose daemon-managed hook integrations without mutation.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when transport, protocol, or decoding fails.
    pub async fn integration_doctor(
        &mut self,
        params: IntegrationDoctorParams,
    ) -> Result<IntegrationDoctorResult, ClientError> {
        self.call::<protocol::method::IntegrationDoctor>(params)
            .await
    }

    /// Remove the managed hooks owned by the installer on the daemon host.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when transport, protocol, or decoding fails.
    pub async fn integration_uninstall(
        &mut self,
        params: IntegrationUninstallParams,
    ) -> Result<IntegrationUninstallResult, ClientError> {
        self.call::<protocol::method::IntegrationUninstall>(params)
            .await
    }
}
