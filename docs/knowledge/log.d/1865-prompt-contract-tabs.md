## Unreleased (2026-11-01, tab- and other white-space-indented lines in the assistant prompt contract, #1865)

- The shared prompt-contract parser treats a tab- or other white-space-indented line in `assistant/system.md` like a space-indented one: a continuation line is joined into its item and a bullet nested by such indentation is refused as a nested list, in both `cargo xtask docs check` and the launch of `pohunek assistant`. See [system.md](assistant/system.md).
