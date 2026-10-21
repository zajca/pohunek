## Unreleased (2026-10-19, worker-side forwarded active identity, #1101)

- Worker protocol 8 adds `RecordActiveIdentity` and `ReleaseActiveIdentity`: the
  controller forwards a public agent report or release into the worker, which
  validates the subject like a hook claim (hook schema, claim expiry, PTY-tree
  membership and start identity of the process, ordering against every earlier
  claim of that process), journals it and acknowledges after the journal write.
  A refusal is an answer, not an error; a journal write failure is a
  non-retryable runtime fault. The daemon sends them for a bound public agent
  report or release (see the entry on durable public agent reports).
- The journal gains the optional claim fields `agent`, `origin` and
  `reported_session` and the optional `active_identity_revision`, which counts
  every change of the active identity or its release, hook or forwarded. The
  journal schema stays 4. A forwarded claim keeps no native reference, so an
  older reader sees a provider-only active claim. A connection that negotiated
  protocol 7 gets a snapshot without the new fields.
- The daemon's projection of a worker's active identity gives a forwarded claim
  the profile label it was reported under when the label resolves to a profile
  of the claim's provider, and otherwise keeps the provider and logs
  `session.active_identity.label_unresolved`. The claim's session annotation
  becomes the active agent's session id or path and never a native reference.
- A forwarded release that names the exact pid and start identity of the
  journaled owner is accepted although that process has exited; any other
  subject must still be a live member of the PTY tree.
