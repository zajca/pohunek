## Unreleased (2026-11-11, Revoked package digest follow-ups, #2099)

- The `package_revocations_unreadable` repair now covers a damaged or newer-schema `revoked-digests.json`: remove it and install the latest release catalog again, which records its revocations anew; a revocation that no later catalog repeats is lost. Updated [runtime-packages](guides/runtime-packages.md) and [runtime-package-archive](concepts/runtime-package-archive.md).
- `command.run` refuses a command of a catalog-authorized package whose digest a recorded catalog revoked with `command_unavailable`. Updated [runtime-packages](guides/runtime-packages.md).
