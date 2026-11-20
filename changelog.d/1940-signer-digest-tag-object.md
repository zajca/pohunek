---
kind: changed
---
The plugin repositories guide now says that the `--signer-digest` of the plugin
archive attestation check is the object the qualified tag ref of the core tag
`source.built_with` points to: for an annotated tag, the tag object, not its
commit. It names the `gh api` lookup and records that the first official entry
passed the check.
([#1940](https://github.com/zajca/pohunek/issues/1940))
