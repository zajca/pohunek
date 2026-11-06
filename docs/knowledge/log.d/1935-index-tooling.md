## Unreleased (2026-11-06, plugin index tooling, #1935)

- New `cargo xtask index build|message|attach|verify|throwaway` commands and the entry file schema in [Plugin index](concepts/runtime-catalog.md#plugin-index); the runbook [Plugin index key ceremony and compromise procedure](runbooks/plugin-index-keys.md) gains the signing flow. The package crate exports `index_document_signing_message`, `verify_key_record_document`, `verify_revocation_document` and `check_index_body`, which the daemon's verifier shares. No daemon, CLI or protocol surface changes.
