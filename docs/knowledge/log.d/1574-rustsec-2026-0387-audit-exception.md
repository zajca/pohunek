## Unreleased (2026-10-24, audit exception RUSTSEC-2026-0387, #1574)

- `.cargo/audit.toml` ignores RUSTSEC-2026-0387 (crypto-bigint 0.5.5, tracked
  in #1575). The crate is locked only through `pohunek-relay` ->
  `openidconnect` 4.0.1 -> p256/p384 0.13, which the relay uses solely to
  verify ID-token ECDSA signatures over public keys and public data; no secret
  EC scalar, signing or ECDH path is reachable. No other shipped binary links it.
- `xtask`'s `dependency_policy` test pins the exact parent edges into
  crypto-bigint 0.5 and the curve stack, and reports the ignore as removable
  once no 0.5 release is locked. `.clippy.toml` bans the p256/p384 secret-key,
  secret-scalar, signing-key and ECDH APIs, so the relay cannot start using
  them.
- Remove the ignore, the policy test and the clippy bans once `openidconnect`
  moves to p256/p384 0.14 (crypto-bigint >= 0.7.6).
