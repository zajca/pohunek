//! The metadata-store lines no loader accepts, on the session registry: the
//! counts the doctor reports.

// Rust guideline compliant 2026-10-29

use tracing::warn;

use super::SessionRegistry;

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
}
