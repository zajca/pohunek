---
kind: changed
---
The daemon journals a provider usage-limit screen when it happens: one info line under `session.program_status.limit_screen` carries the matched marker, the held OSC 7501 record in canonical form, its age and staleness, the latest unmapped record's bounded tokens and the published activity, and never screen or message text. A record the daemon cannot map is now journaled at info rather than debug, so it reaches the default `info` filter of the daemon log; the journal runs only for a runtime that opted in to program status and writes one line per distinct `(reason, state, kind, app)` tuple per session, so repeated records cannot flood the log. ([#2101](https://github.com/zajca/pohunek/issues/2101))
