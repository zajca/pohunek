---
kind: added
---
The `pohunek-plugin-kit` binary (`crates/plugin-kit`) gives plugin repositories
`lint`, `build`, `verify`, `detect-check`, `smoke` and `prove` over one package
directory. `smoke --release-dir <dir> [--upstream <path>]` installs the built
package on an isolated daemon through a throwaway index, waits for detection
past `unknown`, requires a non-empty screen, then stops after the screen check,
and `cargo xtask plugin build` shares the kit logic as a thin wrapper.
The prove evidence carries `kit_sha256`, the digest of the binary the proof
ran in.
