## Unreleased (2026-11-06, trusted checkout arguments, #1925)

- Runtime feature `checkout-trust-args-v1`: a descriptor declares `[runtime] trusted_checkout_args` and `trusted_checkout_paths`, and the daemon appends the arguments at create, resume and fork only when the checkout's origin is a GitHub repository in `service.toml` `[checkout_trust] trusted_repositories` and the declared paths equal the tree of the commit the remote `HEAD` names. See [runtime packages](guides/runtime-packages.md#trusted-checkout-arguments) and [setup](guides/setup.md).
