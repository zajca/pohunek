---
kind: fixed
---
`pohunek plugin install --release <dir> --runtime <id>` without a daemon now also fails as `release_install_incomplete` when the apply leaves another catalog trust state than the preview promised, and its text result lists the host profile the step bound or repinned, as the `--json` result already did.
