//! The private cause of an [`AuthError::Durable`](super::AuthError::Durable).
//!
//! A durable failure reaches clients only as one fixed message. Its cause is
//! kept as an error source so a test failure or an operator can tell a lapsed
//! authority fence from a serialization conflict or a lost connection, without
//! the database detail ever entering the client-facing text.
//!
//! Each link of the chain holds a sanitized description, never the original
//! error: the text of a database error can quote the values of a failed
//! statement, so a `sqlx` error is reduced to its class, `SQLSTATE` code and
//! constraint name, and every other error contributes its own `Display` text,
//! which the relay's error types keep free of detail.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

/// Separator between the links of [`DurableCause::chain`].
const CHAIN_SEPARATOR: &str = ": ";

/// One sanitized link of the cause chain of a durable authentication failure.
///
/// The next link, when there is one, is the [`Error::source`].
#[derive(Debug)]
pub struct DurableCause {
    description: String,
    next: Option<Box<DurableCause>>,
}

impl DurableCause {
    /// Describes `error` and every error in its source chain.
    pub(crate) fn from_error(error: &(dyn Error + 'static)) -> Self {
        let mut descriptions = Vec::new();
        let mut current = Some(error);
        while let Some(link) = current {
            if let Some(database) = link.downcast_ref::<sqlx::Error>() {
                // A database error ends the chain: its sources are transport
                // or driver detail that this summary already classifies.
                descriptions.push(describe_database(database));
                break;
            }
            descriptions.push(link.to_string());
            current = link.source();
        }
        Self::from_descriptions(descriptions)
    }

    /// Describes a failure that has no underlying error value.
    pub(crate) fn from_reason(reason: &'static str) -> Self {
        Self {
            description: reason.to_owned(),
            next: None,
        }
    }

    fn from_descriptions(descriptions: Vec<String>) -> Self {
        let mut next = None;
        for description in descriptions.into_iter().rev() {
            next = Some(Box::new(Self { description, next }));
        }
        match next {
            Some(first) => *first,
            None => Self::from_reason("unknown durable failure"),
        }
    }

    /// Returns every link of the chain joined into one line, outermost first.
    #[must_use]
    pub fn chain(&self) -> String {
        let mut line = self.description.clone();
        let mut next = self.next.as_deref();
        while let Some(link) = next {
            line.push_str(CHAIN_SEPARATOR);
            line.push_str(&link.description);
            next = link.next.as_deref();
        }
        line
    }
}

impl Display for DurableCause {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.description)
    }
}

impl Error for DurableCause {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.next
            .as_deref()
            .map(|next| next as &(dyn Error + 'static))
    }
}

/// Reduces a database error to its class, `SQLSTATE` code and constraint name.
fn describe_database(error: &sqlx::Error) -> String {
    match error {
        sqlx::Error::Database(database) => format!(
            "database error (sqlstate {}, constraint {})",
            database.code().as_deref().unwrap_or("none"),
            database.constraint().unwrap_or("none"),
        ),
        sqlx::Error::Io(_) => "database connection failed".to_owned(),
        sqlx::Error::PoolTimedOut => "database pool timed out".to_owned(),
        sqlx::Error::PoolClosed => "database pool closed".to_owned(),
        sqlx::Error::RowNotFound => "database row not found".to_owned(),
        sqlx::Error::Tls(_) => "database TLS failed".to_owned(),
        sqlx::Error::Protocol(_) => "database protocol error".to_owned(),
        sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) => {
            "database value could not be decoded".to_owned()
        }
        _ => "database operation failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::AuthorityError;
    use crate::store::StoreError;

    #[test]
    fn a_chain_keeps_every_link_in_order_as_error_sources() {
        let error = AuthorityError::Store(StoreError::StaleState);
        let cause = DurableCause::from_error(&error);

        assert_eq!(cause.to_string(), "relay authority is unavailable");
        let second = cause.source().expect("the store error is the next link");
        assert_eq!(
            second.to_string(),
            "relay state changed before the operation completed"
        );
        assert!(second.source().is_none());
        assert_eq!(
            cause.chain(),
            "relay authority is unavailable: relay state changed before the operation completed"
        );
    }

    #[test]
    fn a_reason_is_a_single_link() {
        let cause = DurableCause::from_reason("row was not written");
        assert_eq!(cause.chain(), "row was not written");
        assert!(cause.source().is_none());
    }

    #[test]
    fn a_database_error_is_reduced_to_a_class_without_its_message() {
        let secret = "invalid input syntax for type uuid: \"s3cr3t-token-value\"";
        let cause = DurableCause::from_error(&sqlx::Error::Protocol(secret.to_owned()));
        assert_eq!(cause.chain(), "database protocol error");

        let cause = DurableCause::from_error(&StoreError::Database(sqlx::Error::Protocol(
            secret.to_owned(),
        )));
        let chain = cause.chain();
        assert!(chain.contains("relay database operation failed"), "{chain}");
        assert!(chain.contains("database protocol error"), "{chain}");
        assert!(!chain.contains("s3cr3t"), "{chain}");
        assert!(!format!("{cause:?}").contains("s3cr3t"));
    }

    #[test]
    fn pool_failures_are_classified() {
        assert_eq!(
            DurableCause::from_error(&sqlx::Error::PoolTimedOut).chain(),
            "database pool timed out"
        );
        assert_eq!(
            DurableCause::from_error(&sqlx::Error::RowNotFound).chain(),
            "database row not found"
        );
    }
}
