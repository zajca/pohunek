---
kind: fixed
---
`project.candidates` with the `history` source no longer collects and sorts
every entry of one history directory before applying the entry bound: the
listing keeps at most the remaining budget (the newest names first) and
checks the deadline while listing, and a cut listing is reported as
`entries` or `time` rather than silently partial. History files are now opened
relative to directory fds with `O_NOFOLLOW` on every component, so an
intermediate directory swapped for a symlink between the walk and the read is
refused.
([#2168](https://github.com/zajca/pohunek/issues/2168))
