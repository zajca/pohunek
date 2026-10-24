## Unreleased (2026-10-24, acceptance harness starts D4 sessions on a project, #1112)

- A remote `pohunek session new` needs `--project` (or `--repo`), and a project
  needs a git repository, which the Debian genericcloud image does not have.
  The acceptance harness now stages `git` and the part of its dependency closure
  the image lacks, with `dbus-user-session`, as pinned offline packages
  (sha256-verified, snapshot.debian.org primary), creates a throwaway repository
  with one commit on each VM, registers it with `pohunek project add` and starts
  the D4 session on the peer's project. The zajcaair command shell session
  is started under a project the operator names with `--macos-project` (no
  default, so the owner's working checkout is never chosen implicitly).
- The evidence helper tolerates IPv6 endpoint addresses in a status recording
  (they used to leave invalid JSON and crash `assemble`) and reports an
  unparsable recording as an error; the D7 hint check also reads the `recover`
  text; the D7/D5 extras resolve the concrete daemon unit name before stopping
  it. No product code changes.
