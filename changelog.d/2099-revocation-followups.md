---
kind: fixed
---
The `package_revocations_unreadable` error now names the repair for a damaged revocation file or one a newer release wrote: remove `revoked-digests.json`, then install the latest release catalog again, which records its revocations anew. `plugin list`, `plugin doctor` and `session list` read the recorded revocations once per call instead of once per package or session, so an unreadable file is logged once. `command.run` also refuses a command of a catalog-authorized package whose digest a recorded catalog revoked, with `command_unavailable`.
