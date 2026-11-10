---
kind: added
---
`pohunek integration doctor --agent hermes` adds an optional `note` to a passing `asset_source` check when the selected Hermes package is not the version this pohunek release ships, and names the recovery (`pohunek plugin install --release <extracted daemon archive> --runtime hermes --yes`, then `pohunek integration update --agent hermes`). The note does not fail the check or change `ok`. `scripts/smoke-hermes-plugin-release --no-daemon <sealed release dir> <hermes>` smokes the CLI-only install from a sealed daemon archive.
