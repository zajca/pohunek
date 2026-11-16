---
kind: added
---
`cargo xtask plugin prove` proves context index entries: the install
reports the archive as an enabled, selected official package with no
runtime, the inspect reads the package back, and the assistant entry
renders its prompt through a stub agent profile the rig writes before
the daemon starts. `cargo xtask index throwaway --schema-version 2`
signs a schema 2 document for the context tree. ([#2170](https://github.com/zajca/pohunek/issues/2170))
