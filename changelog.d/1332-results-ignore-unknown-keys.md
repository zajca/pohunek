---
kind: changed
---
A client now ignores a key it does not know in a daemon result, event or error: `ProtocolError` (so an error response from a newer daemon keeps its typed class, code and message instead of becoming a `json_error`), the `session.read`, `session.screen`, `session.output`, `session.wait`, `session.wait_turn` and `session.detection` results, their nested terminal cursor, runtime identity, output gap and turn event and progress records, and the `session.report_native_id`, policy and unconfirmed-process results. The types that deny unknown keys keep doing so (the strict params, persisted capabilities and policies, governance state, the envelope framing). A field is added to these types only after the next protocol bump or when only clients of this release can receive it.
