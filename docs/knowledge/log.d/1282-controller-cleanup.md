## Unreleased (2026-10-19, controller lease cleanup, #1282)

Worker control-connection cleanup releases only the exact owner and lease ID
still held by that connection. A released controller finishing its response
cannot revoke a replacement controller from the same daemon process, and stale
cleanup no longer advances the data-stream lease epoch.
