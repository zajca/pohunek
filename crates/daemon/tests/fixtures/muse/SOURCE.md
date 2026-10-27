# Muse session log fixtures

`session-framed.jsonl` and `session-bare.jsonl` are session logs in the shape of
a Muse Code session store, `<root>/YYYY/MM/DD/<uuid>/session.jsonl`:

- a framed line is a transaction, `{"retained_frame": …, "children":
  [{"record_json": "<escaped record>"}]}`;
- a bare line is the record itself;
- the record with `payload_type` `runtime.session.metadata` carries the session
  id in `stream.id` and the workspace in `payload.record.workspace_root`.

`@SESSION@` and `@WORKSPACE@` are replaced by the scenario that reads a file.

## Source and release

- Derived from the fixture builder and the parser tests of
  [asheshgoplani/agent-deck](https://github.com/asheshgoplani/agent-deck),
  `internal/session/muse_discovery_test.go` and `muse_discovery.go` at commit
  `967e2faf2b98b30da61206baaeff6bf998709f1b` (MIT License, Copyright (c) 2025
  Ashesh Goplani). The record keys and the metadata `recorded_at` value are
  taken from there; no text of the Muse Code program is included.
- Muse release: Muse Code 1.0.2, the release the same agent-deck commit pins its
  screen patterns to. The test file itself records no release, so the shape is
  not re-captured against another one. Re-check the fixtures when a release
  changes the store layout.
- Nothing here comes from a Paradev (ELv2) or Zentty (GPL-3.0) source.
