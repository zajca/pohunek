---
kind: added
---
An installed package can declare commands in `[commands.<name>]`, and `command.run` runs one on the daemon host: a fixed program of the verified package root with a cleared environment, a 60 s and 128 KiB limit, and the program's stdout returned verbatim. The local socket may call every declared command and an overlay peer only a command declared `peers`; installing a package that declares commands needs `accept_commands`. The TypeScript SDK has `runCommand`, which reports `command_unsupported` against a daemon without the method.
