## Unreleased (2026-10-14, final runtime package and workflow plugin docs, #151)

- The workflow plugins guide and `docs/public-api.md` describe event reactions
  as shipped (the dispatcher unit's hidden `pohunek workflow __dispatch`), and
  the guide lists `status` among the commands whose plugin id completes.
- `docs/architecture.md` names `pohunek workflow enable`, `disable` and
  `uninstall` as the commands that drive the plugin-unit contract.
- README and roadmap: workflow plugins (#148) and the package documentation
  (#151) have landed; the open children of #139 are follow-ups.
