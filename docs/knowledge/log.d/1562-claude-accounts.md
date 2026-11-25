## Unreleased (2026-11-23, Claude subscription accounts and package setup entries, #1562)

- Runtime descriptors can export `[setup.<name>]` entries behind the new
  `package-setup-v1` feature: interactive programs of the package that the owner
  runs with `pohunek plugin run <package> <setup> [args...]` on a terminal. The
  daemon only resolves the verified program (`package.setup_resolve`, local and
  owner only) and never runs it. Install review and `plugin inspect` list them.
  See [runtime packages](guides/runtime-packages.md#declaring-package-setup-entries).
- `pohunek plugin profile create --program <abs-path>` attaches an owner-written
  launch wrapper to a host profile; the daemon checks the executable's path,
  owner and permissions before it writes the profile. See
  [agent profiles](concepts/agent-profiles.md#creating-a-profile).
- The Claude package (2.3.0) ships `claude-accounts`, which registers one
  subscription per account (`add <name>`, `--login` for the stored login) and
  creates a `claude-<name>` preset per account, so a session starts under its own
  subscription with `pohunek session new --agent claude-<name>`. See
  [Claude package](guides/claude-package.md#one-claude-subscription-per-session).
- `docs/knowledge/safety/secrets.md`: subscription tokens live in an owner file
  read by the launch wrapper, never in a profile environment.
