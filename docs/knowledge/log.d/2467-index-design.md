## Unreleased (2026-11-22, signed index accepted design, #2467)

- The [plugin index guide](guides/plugin-repositories.md#admission-through-the-index)
  records the six remaining signed-index items as the accepted design: one
  `official` scope with no verified tier (deferred to #1619), key records as
  their own type, entry fields as admission-only, no core cap, compiled index
  URLs beside a channel file that names only the endorser, and mirrors only
  through `--index-url`. The release skill admits the assistant through the
  index after each release: a `CORE_TAG` pull request first, then the
  `pohunek.context.assistant/X.Y.Z` entry.
