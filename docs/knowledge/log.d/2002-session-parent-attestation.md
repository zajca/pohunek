## Unreleased (2026-11-15, attested parent and session.adopt, #2002)

- `session.new` from inside a session records that session as `spawned_by`; markers naming another session are `origin_mismatch`. `session.adopt` (owner only, `session_adopt_supported`) moves a session under a new parent. A profile's `[rights]` table sets `spawn_profiles` and `owner_equivalent` ([Agent profiles](concepts/agent-profiles.md#session-rights), [Trust model](safety/trust-model.md)).
