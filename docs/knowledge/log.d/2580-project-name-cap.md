## Unreleased (2026-11-28, project name cap, #2580)

- A custom project display name (`project add --name`, `project rename`) is
  capped at `MAX_PROJECT_NAME_BYTES` (128 UTF-8 bytes, `crates/protocol`); an
  over-cap name is refused with `bad_request` naming the cap and the given
  length, on the daemon and in the CLI before dialing. One maximal record can no
  longer push a `project.list` reply over the 1 MiB control line.
