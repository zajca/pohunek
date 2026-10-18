## Unreleased (2026-10-19, durable knowledge materialization, #287)

Knowledge bundle materialization synchronizes the extracted files and directories
before publication and the cache directory after the atomic rename. Failed
synchronization is reported and retryable. Incomplete versions are moved aside
within the private cache and rebuilt, while cache cleanup preserves active
extractions and collects abandoned temporary directories. Legacy temporary
directories use a conservative age bound under the shared materialization lock.
See [the assistant architecture](concepts/architecture.md).
