---
type: Runbook
id: runbook/update-after-release
title: Update after release
description: Reconcile setup assets, host capabilities, projects, and launcher config after updating Pohunek.
source_kind: manual
intents: [update, setup, debug, help]
since: 0.3.3
---

# Update After Release

Use this runbook after changing the installed Pohunek binary or rebuilding from
source.

1. Run `pohunek doctor --json` to confirm the current binary can find required
   paths and state directories.
2. Run `pohunek health --json` to confirm the daemon responds with the expected
   version and protocol compatibility.
3. Run `pohunek host inspect local --json` to inspect local runtimes and
   capabilities.
4. Refresh launcher scripts with `pohunek setup scripts`.
5. Review config changes before applying `pohunek setup config --force`; default
   setup config should not overwrite existing files.
6. Reprint or refresh sway integration with `pohunek setup sway --print` or
   `pohunek setup sway`.
7. For important projects, verify `pohunek project show <id-or-label> --json`
   and resolved actions with `pohunek project actions <id-or-label> --json`.

When the assistant feature is available, its update intent should use bundle
version metadata and `changed_in` frontmatter to explain version-specific
changes before recommending edits.
