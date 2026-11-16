---
kind: added
---
`fetch_ref_commit_mismatch` now carries typed `details` naming `expected_commit`, `fetched_commit` and `source_ref`, so a client that sends `fetch_ref` reads the mismatch from `details` instead of parsing `msg`. ([#2365](https://github.com/zajca/pohunek/issues/2365))
