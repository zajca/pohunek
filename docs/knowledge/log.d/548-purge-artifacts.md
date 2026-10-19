## Unreleased (2026-10-19, Purge session-derived artifact copies, #548)

- Service uninstall with `--purge` also removes recognized crashed metadata-store
  write copies and imported legacy-manifest archives. Writers and purge share
  the artifact-name grammar. Recognized temp/archive symlinks are unlinked
  without following their targets; unrelated data entries and pending manifests
  remain and are reported as byte-escaped display strings in `kept_purge_entries`,
  including Unix filenames that are not valid UTF-8.
