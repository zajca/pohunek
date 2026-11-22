---
kind: fixed
---
`pohunek assistant` no longer stops at `assistant_package_missing` with a placeholder `pohunek plugin install <archive> --sha256 <digest>` command. When the daemon reports the assistant context package missing, the CLI plans through the same signed plugin index path as `pohunek setup`: it previews the install, asks once (or proceeds with `--yes`), installs the listed version through daemon verification, and retries the launch once. A remote host names `pohunek assistant setup` for that host and installs nothing remotely. When the index lists no package for the daemon's core, the message says the package is not available for that core yet and to rerun `pohunek assistant setup` later. ([#2511](https://github.com/zajca/pohunek/issues/2511))
