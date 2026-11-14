## Unreleased (2026-11-14, profile presets and fold, #2150)

- A host profile file whose root holds a `[preset]` table is a preset: it
  names a parent profile and a selection of its options (`[preset.options]`,
  optional `locked`), plus `display_name` and `audience`, and launches as the
  parent with the merged selection under its own name and labels. A selection
  sent to a locked preset is `agent_options_locked`. See
  [Agent profiles](concepts/agent-profiles.md#presets).
- A preset session's revision is the parent's followed by the parent name, so
  editing the parent's selected choice or re-parenting needs a decision, while
  the preset's labels, `locked` and choices do not. `package.bind_profile`
  (`plugin profile migrate`) refuses a preset, the release step skips it, and
  `host.inspect` lists it with its labels, its choices as defaults, and no
  options when locked.
- `pohunek plugin profile fold <name>... --into <parent> --dry-run [--json]`
  proposes a parent with `[options]` and one preset per input, with env
  values replaced by placeholders and credential-looking arguments refused.
