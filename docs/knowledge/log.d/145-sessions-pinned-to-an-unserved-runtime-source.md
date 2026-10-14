## Unreleased (2026-10-12, sessions pinned to an unserved runtime source, #145/#146)

- A session is observed, resumed and forked only through the runtime source its
  launch pin names. A built-in pin is served only by a built-in of the same
  package identity the registry serves now; an official package that serves the
  alias, a built-in the daemon does not ship, or a package digest that is not
  installed never stands in. Resume and fork of a built-in session of an alias
  an official package serves fail with `runtime_served_by_package`, whose hint
  names `pohunek session new --agent <runtime>` and states that no session is
  rebound. A live session adopted at startup with such a pin is observed with
  the generic shell manifest (or its host profile's manifest override), its
  persisted input framing and its journaled hook schema; hook reports and
  processes naming its runtime are not attributed through the package, and
  stay unattributed until the next restart even when the pinned source is
  served again. A worker of a release before 0.34.0 journals no hook schema, so
  such a session with a journaled active identity is not adopted
  (`worker_identity_unverified` in the preflight, a `conflict` with
  `active_identity_provider_invalid` at startup). A recovery binding whose
  launch spec the migration left unresolved is completed only from the
  definition its pin names and otherwise stays unrepaired with a
  `native_recovery` warning. The upgrade preflight reports such a session
  `would_lose_recovery` with `runtime_source_not_served`. Documented in
  `guides/runtime-packages`, `concepts/runtime-package-archive` and
  `update-after-release`.
