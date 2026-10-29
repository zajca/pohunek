## Unreleased (2026-10-29, tolerant session result enums, #1337)

- A client of an older build reads a runtime inventory status, retention reason, retention hold, turn-wait reason and read source that a newer daemon adds as `unknown`, drops an unknown detection region from `supported_regions`, and exits `session wait` non-zero when the wait reason is unknown. An unknown retention hold stays a hold, never "free to remove". The daemon's params stay strict and none of these enums is persisted.
