## Unreleased (2026-11-01, naming launch args, #979)

- Runtime feature `naming-launch-args-v1` (0.39.0): a descriptor's `[naming] launch_args` (for example `--name={session_name}`) hands the pohunek session name to the agent at every launch. The placeholder is valid only inside the joined `-<flag>=` token of that table.
- The arguments are rendered per launch (create, resume, fork) from the name that launch uses and are never stored in the resume snapshot, so there is no store or protocol change and `runtime_api` stays 1. See [runtime packages](guides/runtime-packages.md#session-name-arguments).
- No official package declares the table yet; sessions pinned to an earlier package digest launch without the name.
