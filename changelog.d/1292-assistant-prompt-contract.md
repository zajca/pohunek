---
kind: changed
---
`pohunek assistant` renders the safety rules and navigation steps of its opening prompt from the knowledge bundle's `assistant/system.md`, so the prompt and the bundle cannot diverge, and no longer claims a live daemon state or a redacted snapshot: the snapshot file holds launch metadata only and is not redacted, so keep secrets out of the request. The assistant is told never to create or modify hooks in a non-interactive context instead of writing them to a quarantine path that nothing read.
