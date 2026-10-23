## Unreleased (2026-10-23, projector inputs ordered by production, #1507)

An input that ends a notification condition now acts on the records that
existed when the registry produced it, not when the projector got to it. An
approval a provider hook created while the projector was blocked in a slow or
failing session seed is no longer cancelled by the resume produced before it.
