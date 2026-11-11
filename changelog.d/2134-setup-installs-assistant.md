---
kind: added
---
`pohunek setup` installs the assistant package `pohunek.context.assistant` that matches the release, under the same consent, so `pohunek assistant` works right after `setup --yes`. The package comes from the signed plugin index through the resolver of `plugin install`; `setup --dry-run --json` lists it with its digest in `assistant`, the apply refuses an archive whose digest differs from the preview, and the daemon verifies the index signature, revocations, freshness and digest. An unreachable index, a package the index does not list yet, or a document the daemon refuses leaves the package a gap: the other steps finish and `setup` exits with `setup_incomplete` and the manual command from `docs/install.md`. `setup --index-url <url>` replaces the official index.
