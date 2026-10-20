## Unreleased (2026-10-20, installed version directories verified before they run, #1141)

`pohunek service upgrade`, `service install`, a rollback, `service check` and
`pohunek setup` on an installed host verify an installed version directory used as a
source (`--from`, or the directory holding the CLI) or as a rollback or setup
target, with its three binaries, catalog trust anchor and `runtime/` tree,
before anything from it runs or is copied: owned by you, correct modes, no
symbolic links, single-linked and present. A version directory that stopped
being exactly what publishing creates, for example one made group-writable
afterwards, fails with `service_version_untrusted` naming the offending entry
before the daemon, its preflight or the runtime step runs.
