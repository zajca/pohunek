## Unreleased (2026-11-06, plugin build, prove and release workflow, #1936)

- New `cargo xtask plugin build|prove` commands and the reusable `.github/workflows/plugin-release.yml`; the attestation identity that index admission checks is in [Plugin repositories](guides/plugin-repositories.md#admission-through-the-index) and the proof steps and evidence in the [index runbook](runbooks/plugin-index-keys.md#6-build-and-prove-a-plugin-archive). No daemon, CLI or protocol surface changes.
