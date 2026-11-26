## Unreleased (2026-11-26, session.screen links, #2542)

- `session.screen` and `pohunek session screen --json` carry an optional `links`
  array: `http(s)` URLs matched in the visible text (including a URL wrapped at
  the right margin) and OSC 8 hyperlinks, each as `{url, source, row, col}`.
  `host.inspect` reports `session_screen_links_supported`. The links are data
  only; a URL over the configured length is omitted, and an OSC 8 anchor is
  dropped once its text scrolls away or is overwritten. See
  [Sessions](concepts/sessions.md).
