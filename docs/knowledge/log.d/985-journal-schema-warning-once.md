## Unreleased (2026-10-14, unsupported journal warning once per file, #985)

- The daemon logs `worker journal schema is unsupported` at warning level
  once per journal file and schema, and every later read of that file at
  debug level. Each journal scan rereads the journals of every stored session,
  so a host with many old schema-1 journals rotated its daemon log every few
  minutes.
