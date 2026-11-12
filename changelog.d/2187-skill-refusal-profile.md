---
kind: fixed
---
`pohunek integration install --skill` and `integration uninstall --skill` answer `integration_agent_skill_unsupported` for a host profile whose runtime has no daemon-managed hook integration (a profile over Pi), as they already did for `--agent pi`, instead of `agent_not_installable`. The refusal for such a runtime now reads "<runtime or profile NAME> has no daemon-managed hook integration, so the daemon cannot install its agent skill" with the hint "drop --skill: the daemon installs an agent skill only beside hooks it manages"; the hint to install a package version that declares an agent skill location stays with the runtimes that have hooks but no skill location. ([#2187](https://github.com/zajca/pohunek/issues/2187))
