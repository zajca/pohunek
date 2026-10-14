## Unreleased (2026-10-14, one host setup command, #1013)

- Bare `pohunek setup` is the one host setup. It installs the service from
  this release when none is installed, plans the official runtime packages
  for the runtimes this host has (`pohunekd ensure-official-runtimes --dry-run
  --detect`), previews every effect and gap, asks once, then installs the
  packages, pins the profiles, installs the hooks of the homes that exist and
  writes the missing config files. A second plan and the daemon's health and
  `doctor` form the verdict.
- `--yes` applies unattended. Without a terminal and without `--yes`, setup
  refuses with `setup_consent_required`; `--json` needs `--yes` as well. The
  refusal changes nothing.
- An installed service of another version is a gap: `pohunek service upgrade`
  runs first. Setup never upgrades.
- A Hermes home without the pohunek marker is a gap with its install command;
  setup never picks its access policy. Gaps do not fail the verdict.
- A plan that changes after the service install stops with the service
  installed. The stop names the changes the install engine made, and setup
  changes nothing else; a re-run converges. The install engine's own changes
  (installed packages, pinned profiles, a recorded catalog) are accepted only
  as reported by its runtime step.
- A pending interrupted install stops setup before any change.
- A package the install left disabled or unselected is enabled and selected.
- The Hermes homes applied are the homes previewed; the apply does not
  rediscover them.
- `pohunek setup config` and `pohunek setup completions` are unchanged; an
  existing config file is never overwritten.
- `--json --yes` prints one result document that carries the full preview.
  A failure after the first change lists the changes made in its message.
- `pohunek service install` renders the runtime step it ran (catalog, packages,
  profiles).
- A Hermes home previewed as an update that cannot be updated at apply time
  fails the verdict. A config write that fails part way names the files it
  created. A catalog to be recorded on an installed host with no official
  package to install is a note, not an effect.
- Setup installs only the hooks the daemon does not report as current, so a
  run on a ready host applies nothing and a modified hook is still repaired.
- The install quick start, the setup guide and the Homebrew path in the macOS
  runbook use `pohunek setup`.
- The CI job `real systemd supervision` also runs the native setup suite,
  including a fresh host set up from a signed release by one run.
