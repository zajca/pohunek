# Changelog

User-facing changes of pohunek.

## 0.38.1 - 2026-10-28

### Added

- `pohunek host command run <host> <package> <command>` runs a command an installed package declared, on the local daemon or an overlay peer, with `--timeout-ms`, `--max-output-bytes` and `--json`. It prints the program's stdout verbatim and exits 0; each `command_*` error exits with a status of its own (40 to 51, listed in `docs/cli.md`), and `command_unsupported` (40) reports a daemon without the method. The command takes no argument, environment variable, working directory or input. ([#1690](https://github.com/zajca/pohunek/issues/1690))
- An installed package can declare commands in `[commands.<name>]`, and `command.run` runs one on the daemon host: a fixed program of the verified package root with a cleared environment, a 60 s and 128 KiB limit, and the program's stdout returned verbatim. The local socket may call every declared command and an overlay peer only a command declared `peers`; installing a package that declares commands needs `accept_commands`. The TypeScript SDK has `runCommand`, which reports `command_unsupported` against a daemon without the method. ([#1690](https://github.com/zajca/pohunek/issues/1690))

## 0.38.0 - 2026-10-28

### Changed

- The daemon now stops its session registry's background work before it exits, bounded by the optional `deadlines.registry_shutdown_ms` key of `service.toml` (1 to 600000 ms, 5000 ms when absent), and refuses store writes made after that, and a worker no longer rewrites its journal after the terminal outcome is committed except to record the terminal acknowledgement. ([#1592](https://github.com/zajca/pohunek/issues/1592))

## 0.37.1 - 2026-10-27

### Fixed

- `host.inspect` no longer takes seconds on a host with many agent runtimes and a large daemon: version probes spawn without `pre_exec` (so their cost no longer grows with the daemon's memory), run once per distinct program in parallel on a blocking thread, and are cached by the program's file identity for the optional `[inventory]` lifetimes of `service.toml`. `host.inspect` takes an optional `skip_runtime_probes` parameter that answers only the capability flags, which `pohunek session new --cwd`, `--from-remote` and `--input-ack-timeout-ms` now use for their capability check. The Rust `HostInspect` method keeps its `()` parameters; `Client::host_inspect_with` sends the new parameters. The TypeScript SDK types them as `HostInspectParams | null`. ([#1388](https://github.com/zajca/pohunek/issues/1388))
