---
kind: added
---
The daemon installs a package authorized by a signed plugin index: `package.install` accepts the `index` trust kind, verifies the index against the endorser root named by `plugin-index-channel.json` beside the trust anchor, persists the index marks before the install and refuses commands and the runtime ids `shell`, `codex`, `claude`, `hermes` and `pi`. `host.inspect` reports `package_install_index_supported` and `daemon.doctor` reports the `plugin_index_channel` check. ([#1931](https://github.com/zajca/pohunek/issues/1931))
