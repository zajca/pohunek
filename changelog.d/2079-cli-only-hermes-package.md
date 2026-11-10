---
kind: added
---
`pohunek plugin install --release <dir> --runtime <id>` installs an official runtime package from an extracted daemon archive on a host without a daemon, so a CLI-only host can install the Hermes package and then `pohunek integration install --agent hermes`. With a local daemon the install goes over `package.install`; without one, the CLI verifies the sealed archive and runs the release's own `pohunekd ensure-official-runtimes --runtime <id>` once, with the daemon's catalog signature, revocation, freshness and digest checks, and installs no service. `pohunekd ensure-official-runtimes` takes the repeatable `--runtime <id>` flag, which reports the use reason `requested`.
