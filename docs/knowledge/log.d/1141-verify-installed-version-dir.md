## Unreleased (2026-10-20, installed version directories verified before they run, #1141)

`pohunek service upgrade`, `service install`, a rollback, `service check` and
`pohunek setup` on an installed host verify the version directory and its three
binaries before running a binary from it: owned by you, mode `0755`, no
symbolic links, single-linked and present. A version directory that stopped
being exactly what publishing creates, for example one made group-writable
afterwards, fails with `service_version_untrusted` naming the offending entry
before the daemon, its preflight or the runtime step runs.
