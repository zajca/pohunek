## Unreleased (2026-10-14, notifications list pages a long history, #854)

- `pohunek notifications list` without `--limit` lists every matching record
  however long the history is: it requests pages and follows `next_cursor`,
  so it no longer fails with `protocol framing error: control line exceeded
  maximum length`.
- The daemon answers a `notification.list` result that does not fit one
  control line with `notification_list_too_large` and a hint to page with
  `limit` and `next_cursor`, instead of a response the client cannot read. A
  `--limit` too large for one response reports that error.
- A local or remote daemon of an earlier release, which sends such a result
  as one oversized line, is asked again from the first page in smaller pages
  on a new connection, so a listing never combines records from two
  connections.
