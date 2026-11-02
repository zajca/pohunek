---
kind: fixed
---
A session whose name starts with a turn glyph (for example `◐ deploy`) no longer reads as `working` in Claude Code 2.1.309, which sets the bare session name as its terminal title: the detector ignores a title that only repeats the name its launch gave the agent.
