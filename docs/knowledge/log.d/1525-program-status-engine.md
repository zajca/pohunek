## Unreleased (2026-10-23, OSC 7501 program-status engine, #1525)

A detection manifest can opt in to OSC 7501 program-status records with a
`[program_status]` table (`app`, optional `answer_probe`) and the rule flag
`overrides_program_status`. The detector holds the latest root record of that
app and arbitrates it with the screen rules; the evidence is reported as
`state_source = osc_progress`. With `answer_probe = true` the session worker
answers the program's 7501 probe (the daemon sets the new additive
`Initialize` field `answers_program_status_probe`) and drops the attached
terminal's duplicate reply. `session.detection` does not show the record
yet, because older clients cannot decode a new region kind. The Pi and Codex packages opt in
(see their own entries); the other official runtimes behave as
before. See [runtime packages](guides/runtime-packages.md) and
[sessions](concepts/sessions.md).
