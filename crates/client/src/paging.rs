//! The cursor walk `session.list` and `project.list` share.
//!
//! A list method that pages answers records ordered by a key; `cursor` names the
//! key of the last record of the previous page. The walk collects the whole set
//! page by page, so it never has to fit one control line.

use serde::Serialize;

use crate::ClientError;

/// A record a list method pages over.
pub(crate) trait Paged: Serialize {
    /// The cursor that continues the listing after this record. Cursors compare
    /// as strings in the order the daemon sorts the records.
    fn cursor(&self) -> String;
}

impl Paged for protocol::SessionInfo {
    fn cursor(&self) -> String {
        self.id.0.clone()
    }
}

impl Paged for protocol::ProjectInfo {
    fn cursor(&self) -> String {
        self.id.clone()
    }
}

/// Asks a daemon for one page of a list method.
pub(crate) trait PageSource<T> {
    /// One request for at most `limit` records after `cursor`.
    fn fetch(
        &mut self,
        limit: u32,
        cursor: Option<String>,
    ) -> impl std::future::Future<Output = Result<Vec<T>, ClientError>> + Send;
}

/// What distinguishes one paged list method from another.
pub(crate) struct PageWalk {
    /// Records one page asks for.
    pub(crate) page_size: u32,
    /// The code of the daemon's "page does not fit one control line" error.
    pub(crate) too_large_code: &'static str,
    /// The client-raised code for a page that ends at or before its cursor.
    pub(crate) cursor_repeated_code: &'static str,
    /// The client-raised code for a walk past the item or byte bound.
    pub(crate) walk_limit_code: &'static str,
    /// The method name, for messages.
    pub(crate) method: &'static str,
}

/// The `session.list` walk.
pub(crate) const SESSION_LIST: PageWalk = PageWalk {
    page_size: protocol::SESSION_LIST_PAGE_SIZE,
    too_large_code: protocol::ProtocolError::SESSION_LIST_TOO_LARGE,
    cursor_repeated_code: "session_list_cursor_repeated",
    walk_limit_code: "session_list_walk_limit",
    method: "session.list",
};

/// The `project.list` walk.
pub(crate) const PROJECT_LIST: PageWalk = PageWalk {
    page_size: protocol::PROJECT_LIST_PAGE_SIZE,
    too_large_code: protocol::ProtocolError::PROJECT_LIST_TOO_LARGE,
    cursor_repeated_code: "project_list_cursor_repeated",
    walk_limit_code: "project_list_walk_limit",
    method: "project.list",
};

fn client_error(code: &'static str, msg: String, hint: &'static str) -> ClientError {
    ClientError::Protocol(protocol::ProtocolError::new(
        protocol::ErrorClass::Daemon,
        code,
        msg,
        Some(hint.to_owned()),
    ))
}

/// Collects every record of the method by following cursors, asking `source`
/// for one page at a time.
///
/// A page the daemon reports too large is asked again at half the size, down to
/// one record. A daemon that predates paging ignores `limit` and `cursor`, and
/// answers the whole current set to every request: an answer longer than the
/// page size, a first page that is not in ascending cursor order, or one that
/// starts at or before the cursor (a paging daemon never answers a record at or
/// before it) is that set and is returned as it is.
///
/// # Errors
///
/// Returns the error of `source`, or a client-raised error when a page ends at
/// or before its cursor or the walk passes
/// [`protocol::LIST_WALK_MAX_ITEMS`] or [`protocol::LIST_WALK_MAX_BYTES`].
pub(crate) async fn every_page<T, S>(spec: &PageWalk, source: &mut S) -> Result<Vec<T>, ClientError>
where
    T: Paged,
    S: PageSource<T>,
{
    let mut page_size = spec.page_size;
    let mut cursor: Option<String> = None;
    let mut records: Vec<T> = Vec::new();
    let mut bytes = 0_usize;
    loop {
        let page = match source.fetch(page_size, cursor.clone()).await {
            Ok(page) => page,
            Err(ClientError::Protocol(source) | ClientError::RemoteProtocol { source, .. })
                if page_size > 1 && source.code == spec.too_large_code =>
            {
                page_size /= 2;
                continue;
            }
            Err(error) => return Err(error),
        };
        let page_len = page.len();
        let page_cap = usize::try_from(page_size).unwrap_or(usize::MAX);
        if records.is_empty() && page_len > page_cap {
            return Ok(page);
        }
        let (Some(first), Some(last)) = (
            page.first().map(Paged::cursor),
            page.last().map(Paged::cursor),
        ) else {
            break;
        };
        // A paging daemon answers ascending cursor order; the first page of a
        // daemon that predates paging is in its own order.
        if cursor.is_none()
            && page
                .windows(2)
                .any(|pair| pair[0].cursor() >= pair[1].cursor())
        {
            return Ok(page);
        }
        if let Some(cursor) = cursor.as_deref() {
            if first.as_str() <= cursor {
                return Ok(page);
            }
            if last.as_str() <= cursor {
                return Err(client_error(
                    spec.cursor_repeated_code,
                    format!(
                        "the daemon answered a {} page that does not follow its cursor",
                        spec.method
                    ),
                    "upgrade the daemon and client together",
                ));
            }
        }
        bytes = bytes.saturating_add(serde_json::to_vec(&page).map_or(0, |encoded| encoded.len()));
        records.extend(page);
        if records.len() > protocol::LIST_WALK_MAX_ITEMS || bytes > protocol::LIST_WALK_MAX_BYTES {
            return Err(client_error(
                spec.walk_limit_code,
                format!(
                    "the {} walk exceeded the records or bytes a client collects",
                    spec.method
                ),
                "narrow the listing with filters",
            ));
        }
        if page_len < page_cap {
            break;
        }
        cursor = Some(last);
    }
    Ok(records)
}
