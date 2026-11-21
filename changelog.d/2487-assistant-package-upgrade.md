---
kind: fixed
---
`pohunek service upgrade` reports the state of the assistant context package after the swap (`assistant` in `--json`, an `assistant` line in the text output): when the selected version is older than the new core it names the versions and `pohunek setup`, which installs the matching package. `pohunek assistant` prints the same note (and `knowledge_bundle_note` in `--json`) while it launches an older bundle. ([#2487](https://github.com/zajca/pohunek/issues/2487))
