## Unreleased (2026-11-09, relay extraction, #2053, #2054, #2055)

- The team relay (server, HTTPS API, client library, CLI command group and design documents) moved to the separate `pohunekd/pohunek-relay` repository with its history and open issues. [Team relay](concepts/team-relay.md) now states what core keeps: stable host identity, host-local governance, the `RelayId`, relay principal and team types, and the `LaunchSource::RelayProfile` seam. Core release archives no longer contain `pohunek-relayd`, and the relay PostgreSQL CI shard is gone.
