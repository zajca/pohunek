## Unreleased (2026-11-26, project.list paging, #1110)

- `project.list` accepts `limit` and `cursor` and orders projects by label, then id.
  A result that does not fit one control line answers `runtime/project_list_too_large`
  instead of a frame the client refuses; the parameters and the code are additive and
  need no protocol bump.
- The Rust client `project_list` and `pohunek project list` page through the whole
  set, 200 projects at a time, and halve the page when the daemon reports it too
  large. See [Projects](concepts/projects.md).
