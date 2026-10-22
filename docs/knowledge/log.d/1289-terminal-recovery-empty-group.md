## Unreleased (2026-10-22, failed-exec terminal recovery on macOS, #1289)

After a failed `pohunek service lock` exec, the terminal goes back to the
shell when the child's foreground group has no live member. Darwin answers
the liveness probe of an emptied group that the terminal still references with
`EPERM`, not `ESRCH`, so the recovery used to leave the terminal with the dead
group. On macOS an `EPERM` probe now lists the group's members (`libproc`) and the
terminal is taken back only when none is alive; a zombie is not alive, a
membership query that fails keeps the terminal where it is, and a live group
is never taken from. On Linux `EPERM` always means a live member, so the
terminal stays. The failure message gains
`live_members=` with the query result.
