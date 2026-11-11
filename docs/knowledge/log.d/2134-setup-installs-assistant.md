## Unreleased (2026-11-11, setup installs the assistant package, #2134)

- `pohunek setup` resolves `pohunek.context.assistant` for the release through the signed plugin index (schema 2), previews it with its digest in `assistant`, installs it under the same consent and ends a run that could not install it with `setup_incomplete` and the manual command; `setup --index-url` replaces the official index (CLI reference, host setup guide, install guide).
