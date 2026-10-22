## Unreleased (2026-10-22, projector backlog resolve, #1463)

- An input the projector kept while its epoch seed failed acts on what existed
  when the input arrived. When the retry replays it, the turn that opens, the
  agent that resumes or the session that stops no longer acknowledges or
  cancels a record a provider hook created in between, such as the report of the
  turn that just ended or a newer approval request. See
  [sessions](concepts/sessions.md).
