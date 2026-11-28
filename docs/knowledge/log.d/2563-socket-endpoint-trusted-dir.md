## Unreleased (2026-11-28, socket endpoint check on TrustedDir validation, #2563)

- The local socket resolver's private-endpoint check now reuses the
  `pohunek-platform` `TrustedDir` validation (one implementation), so it also
  covers ancestor directories and extended ACLs, not just the entry's own
  symlink, owner and mode bits. The typed client errors are unchanged;
  `UnsafeEndpointReason` gains `UnsafeAncestor` and `UnsafeAcl`.
