---
kind: added
---
A named pohunek session now passes its name to Claude Code (`claude --name=<name>`) at launch, resume and fork, so Claude shows the name in its prompt box and terminal title; the Claude package needs pohunek 0.39. Two live Claude sessions with one name keep both names unique on Claude's side (`<name>-<adjective>-<noun>`).
