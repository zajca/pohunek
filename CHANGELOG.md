# Changelog

User-facing changes of pohunek.

## 0.38.1 - 2026-10-28

### Added

- `pohunek host command run <host> <package> <command>` runs a command an installed package declared, on the local daemon or an overlay peer, with `--timeout-ms`, `--max-output-bytes` and `--json`. It prints the program's stdout verbatim and exits 0; each `command_*` error exits with a status of its own (40 to 51, listed in `docs/cli.md`), and `command_unsupported` (40) reports a daemon without the method. The command takes no argument, environment variable, working directory or input. ([#1690](https://github.com/zajca/pohunek/issues/1690))
- An installed package can declare commands in `[commands.<name>]`, and `command.run` runs one on the daemon host: a fixed program of the verified package root with a cleared environment, a 60 s and 128 KiB limit, and the program's stdout returned verbatim. The local socket may call every declared command and an overlay peer only a command declared `peers`; installing a package that declares commands needs `accept_commands`. The TypeScript SDK has `runCommand`, which reports `command_unsupported` against a daemon without the method. ([#1690](https://github.com/zajca/pohunek/issues/1690))

## 0.38.0 - 2026-10-28

### Added

- A runtime descriptor can list core capabilities in a top-level `requires` array. The core refuses a descriptor that needs a capability it does not implement, or that uses a gated key without it, with `runtime_feature_unsupported`. ([#1612](https://github.com/zajca/pohunek/issues/1612))
- Runtime packages can describe Muse Code and other out-of-tree runtimes: numeric `1.4.13-R2` version probes (`version-probe-revision-v1`), literal `[launch_env]` flags from a closed set (`launch-env-v1`), a config home below a variable's value (`config-home-subdir-v1`), session logs below a data root read with the `muse-session-v1` parser (`transcripts-data-root-v1`), and the `muse-hook-v1` hook handler with a session-private managed hooks overlay (`integration-muse-hook-v1`). The Muse hook payload fixtures are derived from documentation and marked unverified. ([#1645](https://github.com/zajca/pohunek/issues/1645)) ([#1648](https://github.com/zajca/pohunek/issues/1648)) ([#1649](https://github.com/zajca/pohunek/issues/1649)) ([#1667](https://github.com/zajca/pohunek/issues/1667))
- An integration handler can contribute per-launch input (environment pairs, root-position arguments and private files) that core validates and applies to every create, resume and fork without storing it in the resume snapshot. A refused contribution fails the launch with `runtime/launch_input_invalid` or `runtime/launch_input_unavailable`. ([#1648](https://github.com/zajca/pohunek/issues/1648))
- `session.new` accepts `secret_env`, a map of variable name to value for the agent process. The host profile declares the names (`secret_env = ["NAME"]`) and the request supplies only the values, which are never persisted, listed or logged, so resume, fork and relay launches of such a profile fail with `secret_env_missing`. Clients check the new `HostCapabilities.session_new_secret_env_supported` first. ([#1691](https://github.com/zajca/pohunek/issues/1691))
- A package descriptor can declare commands (`[commands.<name>]` with `program`, `args` and `expose = "local" | "peers"`) behind `requires = ["package-commands-v1"]`. Installing such a package needs the `accept_commands` opt-in, advertised as `HostCapabilities.package_install_commands_supported`; without it the request is refused with `package_commands_client_unsupported`. The install review shows every command, and nothing runs yet: `command.run` arrives in 0.38.1. ([#1690](https://github.com/zajca/pohunek/issues/1690))
- On Linux the worker journals its cgroup path (`worker_control_group`, optional, no schema bump), so the lost-runtime sweep ignores bystander processes in other cgroup scopes and `session rm` no longer refuses with `runtime_supervision_ambiguous` because of them. ([#186](https://github.com/zajca/pohunek/issues/186))

### Changed

- The daemon now stops its session registry's background work before it exits, bounded by the optional `deadlines.registry_shutdown_ms` key of `service.toml` (1 to 600000 ms, 5000 ms when absent), and refuses store writes made after that, and a worker no longer rewrites its journal after the terminal outcome is committed except to record the terminal acknowledgement. ([#1592](https://github.com/zajca/pohunek/issues/1592))

## 0.37.1 - 2026-10-27

### Fixed

- `host.inspect` no longer takes seconds on a host with many agent runtimes and a large daemon: version probes spawn without `pre_exec` (so their cost no longer grows with the daemon's memory), run once per distinct program in parallel on a blocking thread, and are cached by the program's file identity for the optional `[inventory]` lifetimes of `service.toml`. `host.inspect` takes an optional `skip_runtime_probes` parameter that answers only the capability flags, which `pohunek session new --cwd`, `--from-remote` and `--input-ack-timeout-ms` now use for their capability check. The Rust `HostInspect` method keeps its `()` parameters; `Client::host_inspect_with` sends the new parameters. The TypeScript SDK types them as `HostInspectParams | null`. ([#1388](https://github.com/zajca/pohunek/issues/1388))
