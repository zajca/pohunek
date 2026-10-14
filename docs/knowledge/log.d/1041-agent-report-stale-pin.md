## Unreleased (2026-10-14, agent reports follow the current pin, #1041)

- An active-agent report resolves its agent against the session's pinned
  definition, or through the runtime registry for an agent of another runtime,
  and commits under the sessions lock. When a package transaction replaced that
  definition in between (an install, an uninstall or a version change), the
  report now resolves again against the current pin and registry instead of
  committing the replaced definition's detector rules and input readiness. A report that keeps meeting replaced
  pins is ignored after three attempts.
- A procwatch scan is discarded, and procwatch scans again, when a package
  change replaced a definition it matched with during the scan: the session's
  own pin, or the runtime registry that other runtimes' agents resolve
  through. When the pin changes, the
  agents procwatch already observed through the replaced definition move to
  the current one. With no current definition they are no longer attributed
  to the session.
