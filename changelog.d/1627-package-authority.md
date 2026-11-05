---
kind: changed
---
The daemon's runtime package lifecycle (install, link, enable, select, uninstall, profile bind) now lives in its own `PackageAuthority` under `crates/daemon/src/packages/` instead of the session registry. No behavior or wire change. ([#1627](https://github.com/zajca/pohunek/issues/1627))
