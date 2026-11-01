## Unreleased (2026-11-02, Claude package session name, #979)

- Claude package 1.2.0 declares `[naming] launch_args = ["--name={session_name}"]` (`requires = ["naming-launch-args-v1"]`), so a named pohunek session runs `claude --name=<name>` at launch, resume and fork. The package needs a 0.39 core.
- Recorded on Claude Code 2.1.289 and 2.1.309: the name is shown in the prompt box rule and the terminal title (2.1.309 sets the bare name as title, 2.1.289 keeps its state glyph), resume shows the current name, a fork its own, and a colliding name is renamed by Claude to `<name>-<adjective>-<noun>`. See [Claude package](guides/claude-package.md#session-name).
