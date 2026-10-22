## Unreleased (2026-10-22, session new --cwd, #1399)

`session new --cwd` takes effect for a project-bound session: with `--project`
or `--repo` the session starts in the requested directory, inside the checkout or
anywhere else on the target host, and keeps the project association of the
reference. A missing or non-directory path, a relative remote path and `--cwd`
with `--branch` are refused before a session is created, and a host whose daemon
does not advertise `session_new_cwd_supported` is refused with
`session_new_cwd_unsupported` for every explicit `--cwd`. The daemon stores the
canonical launch directory. A by-reference session launched outside every
repository keeps its project while it moves between directories outside every
repository; one launched inside a repository follows its cwd as before. See [sessions](concepts/sessions.md).
Without `--project`/`--repo`, an explicit `--cwd` below a checkout now starts the
session in that directory; the CLI sends the work-tree root itself for a start
without `--cwd`. An older CLI with this daemon sends its own directory for such
a start, which now starts in that directory instead of the checkout root.
An older CLI sends its own directory as `cwd` for every start on the same host,
also with `--project` or `--repo`, and sends no `cwd` to a remote host. After a
partial upgrade, an older CLI against this daemon on the same host therefore
starts a `--project` session in the caller's directory instead of the project
checkout. `pohunek service upgrade` updates both.
