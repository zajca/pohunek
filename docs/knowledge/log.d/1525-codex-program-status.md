## Unreleased (2026-10-23, Codex 0.164.0 and OSC 7501 opt-in, #1525)

The Codex package is verified on Codex 0.164.0 and opts in to OSC 7501 program
status (`app = "codex"`, no probe answer: Codex reports on its own). Codex
0.163.0 and later therefore report working, idle and blocked through the
record; a background terminal that outlives its turn reads idle
(`background_terminals_idle`). The captures of 0.160.0, the supported minimum,
stay as the previous set. The package drops the rules for strings no Codex
release draws (`workspace_trust_prompt`, `allow command?`, `yes (y)`). See the
[Codex package guide](guides/codex-package.md).
