# Changelog

User-facing changes of pohunek.

## 0.38.0 - 2026-10-28

### Changed

- The daemon now stops its session registry's background work before it exits, bounded by the optional `deadlines.registry_shutdown_ms` key of `service.toml` (1 to 600000 ms, 5000 ms when absent), and refuses store writes made after that, and a worker no longer rewrites its journal after the terminal outcome is committed except to record the terminal acknowledgement. ([#1592](https://github.com/zajca/pohunek/issues/1592))

## 0.37.1 - 2026-10-27

### Fixed

- `host.inspect` no longer takes seconds on a host with many agent runtimes and a large daemon: version probes spawn without `pre_exec` (so their cost no longer grows with the daemon's memory), run once per distinct program in parallel on a blocking thread, and are cached by the program's file identity for the optional `[inventory]` lifetimes of `service.toml`. `host.inspect` takes an optional `skip_runtime_probes` parameter that answers only the capability flags, which `pohunek session new --cwd`, `--from-remote` and `--input-ack-timeout-ms` now use for their capability check. The Rust `HostInspect` method keeps its `()` parameters; `Client::host_inspect_with` sends the new parameters. The TypeScript SDK types them as `HostInspectParams | null`. ([#1388](https://github.com/zajca/pohunek/issues/1388))
