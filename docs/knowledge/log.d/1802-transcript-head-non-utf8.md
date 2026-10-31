## Unreleased (2026-10-31, transcript head scan skips non-UTF-8 lines, #1802)

- The external observer's `jsonl-head-v1` parser reads each line of the scan window as bytes. A line that is not valid UTF-8, or that the 64 KiB limit cuts inside a multibyte character, is skipped like an unparseable line; before, it failed the whole file and the transcript got no attribution.
