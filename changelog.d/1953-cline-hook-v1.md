---
kind: added
---
The Cline CLI package can keep a Pohunek session in the terminal's process tree and report its identity and subagents: runtime feature `integration-cline-hook-v1` adds the `cline-hook-v1` integration handler, which launches Cline in sandbox mode on the user's data root, follows `/clear` and `/fork` at the next turn, journals subagents, and installs one reporter as the six Cline hook files in `<CLINE_DIR>/hooks/`. `pohunek integration install` refuses, and leaves untouched, a file that is not Pohunek's at one of those paths, and `uninstall` removes only Pohunek's files.
