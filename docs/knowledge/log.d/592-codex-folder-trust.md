## Unreleased (2026-10-14, Codex folder trust dialog is blocked, #592)

- The official Codex package's detection manifest gains `folder_trust_prompt`.
  The `Trust this folder?` dialog that Codex 0.160 shows for a working
  directory with a project-local `.codex/config.toml` now reads as `blocked`
  from the screen at terminal widths of 30 columns and more. The rule needs
  the dialog's `Folder access` heading line, the question and both choices, so
  a reply quoting the dialog is not read as it. Below that width the question
  wraps and the session keeps the byte-activity fallback.
- The rule for the older first-run wording is unchanged, and the package keeps
  every rule of the v0.34.2 built-in manifest.
