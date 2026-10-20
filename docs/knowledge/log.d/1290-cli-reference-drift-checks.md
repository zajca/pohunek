## Unreleased (2026-10-20, CLI reference checked against the command tree, #1290)

`cargo xtask docs check` holds the command examples of `README.md`,
`AGENTS.md`, `docs/` and `.agents/` to the clap tree (`doc-commands`), requires
`docs/cli.md` to show every leaf command (`cli-reference-coverage`), and
compares its list of commands without `--json` with the tree
(`json-exceptions`). `docs/cli.md` documents the `relay`, `migration`,
`subscribe`, `session read|resize|metadata|runtime-inventory|policy|retention`
and `assistant project|update` commands, and the Hermes install command is
`pohunek integration install --agent hermes` in
[setup](guides/setup.md) and the CLI guide.
