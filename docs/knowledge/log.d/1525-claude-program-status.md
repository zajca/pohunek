## Unreleased (2026-10-23, Claude Code 2.1.309 and program status, #1525)

The Claude package verifies Claude Code 2.1.309 (lock, npm tree, captures and
the UserPromptSubmit golden) and keeps the 2.1.289 captures as the previous
verified release in `compat/claude/screens/n-1/`. Its manifest opts in to OSC
7501 program-status records (`app = "claude-code"`, `answer_probe = true`): a
Claude session reports working, blocked and idle from Claude's own records,
a visible dialog still wins, and the API-key dialog is a visible blocker. The
spinner rule needs one verb word and an optional timer, so a transcript bullet
no longer reads as a running turn, and the process matchers accept the native
installer's version-named process. Package version 1.1.0.
