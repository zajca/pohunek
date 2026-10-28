## Unreleased (2026-10-28, session.new secret_env, #1691)

- `session.new` accepts `secret_env`, a map of variable name to value for the agent process. The host profile declares the names (`secret_env = ["NAME"]`), the request supplies only the values, and every declared name is required on every launch. The values are never persisted, listed or logged, so resume, fork and relay launches of such a profile fail with `secret_env_missing`. Clients check `HostCapabilities.session_new_secret_env_supported` first. See [Agent profiles](concepts/agent-profiles.md) and [secrets](safety/secrets.md).
