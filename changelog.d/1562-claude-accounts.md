---
kind: added
---
Runtime packages can export setup entries (`[setup.<name>]`, feature `package-setup-v1`): owner-run interactive programs of the package, such as a sign-in flow. `pohunek plugin run <package> <setup> [args...]` runs one on your terminal with a clean environment and the program's exit status; the daemon only resolves it and never runs it. `pohunek plugin profile create --program <abs-path>` attaches an owner-written launch wrapper to a host profile. The Claude package 2.3.0 ships `claude-accounts`: `pohunek plugin run pohunek.runtime.claude accounts add <name>` stores one subscription per account, and `pohunek session new --agent claude-<name>` starts a session under it. ([#1562](https://github.com/zajca/pohunek/issues/1562))
