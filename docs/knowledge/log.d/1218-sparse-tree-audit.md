## Unreleased (2026-10-17, the nightly run checks the test selection's file map, #1218)

- The nightly CI run includes a test selection audit. It runs every
  package's tests in a checkout that holds only the files whose change
  selects that package. A test that reads any other file fails the audit, and
  so does a package that runs fewer tests than in the full checkout. The audit
  is required nightly evidence for a release.
