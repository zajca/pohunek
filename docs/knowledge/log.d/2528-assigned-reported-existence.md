## Unreleased (2026-11-25, existence check for reported ids of assigned runtimes, #2528)

- `reference_existence` in `crates/daemon/src/session/resume.rs` returns the
  frozen assigned existence check for a `reported` reference of an `assigned`
  runtime, falling back to the hook runtime's declared check. Resume, the
  stored-binding preflight, the activity lookup and the upgrade preflight
  share it, so a reported id whose conversation file is gone is refused with
  `agent_native_reference_missing`. The upgrade preflight judges such a
  missing file `already_not_resumable`, because the running release only
  starts an empty conversation under it (`reported_check_identity` in
  `reconcile/upgrade_preflight.rs`). Covered by
  `recovery_checks_an_assigned_reference_and_a_reported_one`
  in `crates/daemon/src/session/tests/native_supersede.rs`. Docs: the
  `sessions.md` assigned-reference paragraph and the `runtime-packages.md`
  integration paragraph.
