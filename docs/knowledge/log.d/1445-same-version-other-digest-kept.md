## Unreleased (2026-10-22, release package under the same version and another digest, #1445)

- `pohunek service upgrade`, `service check` and `pohunekd ensure-official-runtimes`
  no longer plan an install that the registry refuses with
  `package_identity_installed`. When the registry records the release package's id
  and version under another digest and an enabled, loadable selection serves the
  runtime, the plan and the apply keep that selection, write nothing and report the
  gap `plugin_update_same_version_other_digest`. Its detail names the uninstall
  command of the recorded package only where that is proven to work (the selected
  package of a reserved runtime that no profile or session pins), and otherwise says the
  fix is a release with a new version. The gap code is additive: an older client reads it as an unknown
  string.
- Docs: `install.md`, `public-api.md` and the update runbook.
