---
kind: added
---
`pohunek plugin install` accepts `--index <file>` beside `--sha256` and `--catalog`, and installs by package id: `pohunek plugin install <package-id>[@<version>]` fetches the official plugin index (or `--index-url`) and the archive it lists, checks the digest and lets the daemon verify both. The review names the resolved version and the command to repeat pins it. `plugin update` accepts `--index <file>`. ([#1932](https://github.com/zajca/pohunek/issues/1932))
