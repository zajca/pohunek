---
kind: added
---
A runtime can declare `[runtime] trusted_checkout_args` and `trusted_checkout_paths` (feature `checkout-trust-args-v1`): the daemon appends the arguments at create, resume and fork only for a clean checkout of a GitHub repository listed in the new `service.toml` `[checkout_trust] trusted_repositories`, whose declared paths equal the tree of the remote `HEAD` commit. Any git error or missing fact withholds them. ([#1925](https://github.com/zajca/pohunek/issues/1925))
