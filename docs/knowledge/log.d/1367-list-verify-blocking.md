## Unreleased (2026-10-30, session list resolves recovery off the async executor, #1367)

- `session list` and `session inspect` resolve each listed session's recovery definition, which reads the registry record and verifies the package root, on the blocking pool together with the transcript walk. A listing still verifies each distinct package root once and remembers nothing between calls, so a package root changed after one list is refused by the next list and by resume.
