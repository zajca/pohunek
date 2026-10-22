## Unreleased (2026-10-22, failed-exec terminal recovery on macOS, #1289)

After a failed `pohunek service lock` exec, the terminal goes back to the
shell when the child's foreground group has no live member. On macOS the
liveness probe `killpg(group, 0)` ends in `killpg1` (`bsd/kern/kern_sig.c`),
which skips zombies and system processes and answers `EPERM` for a group it
found but counted no member of; that is also the answer for a live group of
another user. The recovery used to treat only `ESRCH` as an empty group, so a
group left with only a zombie kept the terminal. On macOS an `EPERM` probe now
lists the group's members (`libproc`) and the terminal is taken back only when
none is alive; a zombie is not alive, a membership query that fails keeps the
terminal where it is, and a live group is never taken from. On Linux `EPERM`
means a live member of another user, so the terminal stays. The failure
message gains `live_members=` and `group_members=` (each member with its
state), which show what a group still held when the recovery did not reclaim.
The member the macOS CI run actually saw is not established: the reclaim
covers a group with zombie members, and any other member kind still keeps the
terminal and is reported in `group_members=`.
