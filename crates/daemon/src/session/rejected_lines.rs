//! The metadata-store lines no loader accepts, on the session registry: the
//! bounded list `store.rejected_list` returns and the set-aside behind
//! `store.rejected_set_aside`.

// Rust guideline compliant 2026-10-29

use std::io;

use protocol::{
    ProtocolError, StoreRejectedListResult, StoreRejectedSetAsideParams,
    StoreRejectedSetAsideResult, STORE_REJECTED_LIST_MAX_LINES,
};
use tracing::warn;

use super::SessionRegistry;
use crate::store::SetAsideError;

/// The rejected lines the metadata store holds, as the doctor reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectedLinesState {
    /// The store was read.
    Counted {
        /// Lines no loader accepts.
        total: usize,
        /// Of those, the lines that may pin a runtime package.
        may_pin: usize,
    },
    /// The store could not be read.
    Unreadable,
}

/// Maps a store failure to the typed read error, logging the cause.
fn read_failed(error: &io::Error) -> ProtocolError {
    warn!(error = %error, "the metadata store cannot be read for its rejected lines");
    ProtocolError::store_read_failed()
}

impl SessionRegistry {
    /// What the doctor reports about the rejected lines of the store.
    pub(crate) async fn rejected_lines_state(&self) -> RejectedLinesState {
        let Some(store) = self.inner.store.clone() else {
            return RejectedLinesState::Counted {
                total: 0,
                may_pin: 0,
            };
        };
        match tokio::task::spawn_blocking(move || store.rejected_lines()).await {
            Ok(Ok(lines)) => RejectedLinesState::Counted {
                total: lines.len(),
                may_pin: lines.iter().filter(|line| line.may_pin).count(),
            },
            Ok(Err(error)) => {
                warn!(error = %error, "the metadata store cannot be read for its rejected lines");
                RejectedLinesState::Unreadable
            }
            Err(join_error) => {
                warn!(error = %join_error, "the rejected-lines count task failed");
                RejectedLinesState::Unreadable
            }
        }
    }

    /// The rejected lines of the metadata store, at most
    /// [`STORE_REJECTED_LIST_MAX_LINES`] of them, with the total.
    ///
    /// A registry without a store has none.
    ///
    /// # Errors
    ///
    /// `store_read_failed` when the store cannot be read or its schema is
    /// refused.
    pub async fn store_rejected_list(&self) -> Result<StoreRejectedListResult, ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(StoreRejectedListResult {
                lines: Vec::new(),
                total: 0,
            });
        };
        let mut lines = tokio::task::spawn_blocking(move || store.rejected_lines())
            .await
            .map_err(|join_error| read_failed(&io::Error::other(join_error.to_string())))?
            .map_err(|error| read_failed(&error))?;
        let total = u32::try_from(lines.len()).unwrap_or(u32::MAX);
        lines.truncate(STORE_REJECTED_LIST_MAX_LINES);
        Ok(StoreRejectedListResult { lines, total })
    }

    /// Moves the rejected lines with the requested content hash into the
    /// quarantine file; see [`crate::store::Store::set_aside_rejected`].
    ///
    /// # Errors
    ///
    /// `store_rejected_line_not_found`, `store_rejected_line_live` and
    /// `store_quarantine_failed`.
    pub async fn store_rejected_set_aside(
        &self,
        params: StoreRejectedSetAsideParams,
    ) -> Result<StoreRejectedSetAsideResult, ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Err(ProtocolError::store_rejected_line_not_found());
        };
        let outcome = tokio::task::spawn_blocking(move || {
            store.set_aside_rejected(&params.content_hash, params.accept_pin_loss)
        })
        .await
        .map_err(|join_error| {
            warn!(error = %join_error, "the set-aside task failed");
            ProtocolError::store_quarantine_failed()
        })?;
        match outcome {
            Ok(set_aside) => Ok(StoreRejectedSetAsideResult {
                content_hash: set_aside.content_hash,
                lines: set_aside.lines,
                quarantine_file: pohunek_paths::REJECTED_QUARANTINE_NAME.to_owned(),
            }),
            Err(SetAsideError::NotFound) => Err(ProtocolError::store_rejected_line_not_found()),
            Err(SetAsideError::Live) => Err(ProtocolError::store_rejected_line_live()),
            Err(SetAsideError::Failed(error)) => {
                warn!(error = %error, "setting a rejected metadata-store line aside failed");
                Err(ProtocolError::store_quarantine_failed())
            }
        }
    }
}
