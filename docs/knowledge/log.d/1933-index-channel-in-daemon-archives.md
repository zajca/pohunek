## Unreleased (2026-11-06, plugin index channel file in daemon archives, #1933)

- Daemon archives carry `plugin-index-channel.json` beside the trust anchor; `service install` and `upgrade` install it and treat it like the anchor, with the new codes `service_staged_index_channel_invalid` and `service_version_lacks_index_channel`. See [Runtime catalog](concepts/runtime-catalog.md) and [Update after release](runbooks/update-after-release.md#plugin-index-channel-file-from-040).
