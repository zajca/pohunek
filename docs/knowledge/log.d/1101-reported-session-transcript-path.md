## Unreleased (2026-10-20, session annotation with a transcript path, #1101)

- The session annotation a forwarded claim carries (`reported_session`) gains the
  optional `transcript_path`, which rides beside an id annotation and is omitted
  when unset. The worker bounds and journals it with the annotation, and the
  daemon's projection of a forwarded claim sets both `active_agent_session_id`
  and `active_agent_session_path` from it, so a report that names a provider
  session id and its transcript keeps both public fields. A path annotation
  carries no second path. The journal schema stays 4 and an older reader or
  writer ignores or omits the field.
