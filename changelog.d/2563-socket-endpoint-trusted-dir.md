---
kind: changed
---
The local socket resolver's private-endpoint check now runs on the
`pohunek-platform` `TrustedDir` validation instead of a subset of it: one
implementation (in `pohunek_platform::socket_endpoint` /
`filesystem::verify_private_socket_endpoint`), now including the ancestor
directory policy and extended-ACL checks, not just the entry's own symlink,
owner and mode bits (#2563). An existing runtime directory or socket is refused
with the same typed `local_socket_unsafe` error as before when it is unsafe, and
now also when an ancestor directory is unsafe (reported as the new
`UnsafeAncestor` reason) or the entry carries an unsafe ACL (`UnsafeAcl`); the
`pohunek-client` error classes and codes are unchanged. A missing entry still
passes, so connecting reports an absent daemon. Compatibility: a runtime
directory below a group/world-writable ancestor (or with an unsafe ACL) that
the previous subset check accepted is now refused before connecting.
([#2563](https://github.com/zajca/pohunek/issues/2563))
