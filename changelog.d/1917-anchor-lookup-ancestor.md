---
kind: fixed
---
`pohunek setup` works on a stock Homebrew install on macOS. The daemon now accepts a catalog trust anchor beside its executable in a user-owned directory chain that macOS `admin` can write (the stock Homebrew `Cellar`), as the executable lookup already does; the file checks, root and Linux stay strict. A refused `release-catalog` step now says whether the trust anchor is missing or cannot be trusted.
