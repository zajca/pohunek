## Unreleased (2026-10-13, native-id report attestation, #868)

- `session.report_native_id` on the local control socket is attested with the
  caller's kernel peer credentials before any session state is read. Only the
  session's PTY root itself is admitted; a descendant, another user, a remote
  TCP caller, or any report for an `identity-v1` (Hermes) session is refused
  with the error `native_report_unattested` (class `runtime`) and logged once as
  `session.native_report.unattested` with a `reason`.
- Shipped Claude and Codex hook fallbacks to this method are refused, because
  their hooks are children of the agent; the native reference is recorded
  through the worker hook socket. The hook scripts are unchanged.
- The rule is in the public API ("Native-id report attestation"), the sessions
  concept and the trust model.
