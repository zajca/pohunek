---
kind: added
---
Daemon archives ship `plugin-index-channel.json` beside the trust anchor, and `pohunek service install` and `upgrade` copy it into the version directory like the anchor. A version directory published by an older CLI without the file is reported as `service_version_lacks_index_channel` when a 0.40 CLI runs the same version again; installs by plugin id stay off for it until the next release or a reinstall from the archive. ([#1933](https://github.com/zajca/pohunek/issues/1933))
