---
kind: changed
---
The default screen model is now libghostty-vt: every default build renders `session.screen` results and attach repaints with the libghostty backend (`default = ["libghostty"]` in `crates/terminal`, forwarded by the CLI, daemon, session worker and xtask; the CI and release lanes provide the pinned library through the `libghostty-vt` action). The vt100 product remains as the `--no-default-features` fallback build — no Zig toolchain, no C library — and reports `screen_model: "vt100"`. No protocol version bump was needed: `screen_model`/`format_version` stay optional on the wire, so older workers, daemons and clients keep working unchanged. Rollback is one line: `default = []` in `crates/terminal/Cargo.toml`. ([#2538](https://github.com/zajca/pohunek/issues/2538))
