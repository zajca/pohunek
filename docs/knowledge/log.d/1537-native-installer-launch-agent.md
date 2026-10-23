## Unreleased (2026-10-23, native-installer launch agent, #1537)

The `NamedForProvider` launch-agent rule also designates a process whose
symlink-resolved executable is a release image of the provider's native
installer, `<provider>/versions/<release>` (Claude Code's
`~/.local/share/claude/versions/2.1.309`). Before, only an executable file name
containing the provider name qualified, so `SessionStart` identity binding
failed for a native-layout Claude.
