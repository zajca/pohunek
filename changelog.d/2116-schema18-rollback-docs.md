---
kind: fixed
---
The rollback notes for the metadata store schema 18 now name the backup the migration really writes, `metadata.jsonl.pre-schema-<old>`, where `<old>` is the oldest schema it found (`-17` from 0.41, `-15` from 0.40.x), and say that sessions created after the upgrade are lost on rollback. The knowledge log records the schema 18 bump, its no-op migration and the rollback. ([#2116](https://github.com/zajca/pohunek/issues/2116))
