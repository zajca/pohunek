---
kind: fixed
---
A `session new` whose reply times out now tells a caller who passed `--request-id` to retry with that id, and tells one who did not to look the session up by `--meta` and to pass an id on the first create, instead of one text for both. An assistant launch whose outcome stays unknown keeps the error's own recovery hint and appends the request-id text, and on a remote host points at `details.session_id` of the `session_request_conflict` a repeat answers with. A keyed `session.new` now copies only the stored records that hold its key instead of every session record.
