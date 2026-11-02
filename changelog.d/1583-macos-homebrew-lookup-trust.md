---
kind: fixed
---
On macOS, Pohunek now finds agent CLIs, overlay CLIs and `git` in a stock Homebrew prefix: a directory you own that belongs to the `admin` group and that others cannot write (such as `/opt/homebrew/bin` and `Cellar`) is accepted for the executable lookup. Root-owned directories, other groups, world-writable directories and group-writable executable files are still refused. Another admin account on the same Mac can swap entries in those directories, so a host with several admins should run `chmod g-w` on its Homebrew directories. An existing service keeps its recorded `PATH` until `pohunek service refresh-path`; afterwards a Homebrew `git` earlier in the `PATH` shadows `/usr/bin/git`, as in your shell. Linux is unchanged.
