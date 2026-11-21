---
kind: fixed
---
The macOS plugin kit bundle is now built with `packaging/macos/build-release kit`, the same locked, deployment-target-pinned, path-remapped build as the core macOS archives, and its staged tree must pass `audit-macho` before the archive is written. ([#2407](https://github.com/zajca/pohunek/issues/2407))
