## Unreleased (2026-11-01, tab-indented lines in the assistant prompt contract, #1865)

- The shared prompt-contract parser treats a tab-indented line in `assistant/system.md` like a space-indented one: a continuation line is joined into its item and a tab-nested bullet is refused as a nested list, in both `cargo xtask docs check` and the launch of `pohunek assistant`. See [system.md](assistant/system.md).
