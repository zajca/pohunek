---
kind: added
---
`session.screen` (and `pohunek session screen --json`) now reports the links on the screen in an optional `links` array: `http(s)` URLs found in the text, including one wrapped at the right margin, and OSC 8 hyperlinks, each with its `source` and first cell. A client such as a GUI can offer a dev-server address without scraping the text. The links are data only and the text output is unchanged; `host.inspect` advertises `session_screen_links_supported`. ([#2542](https://github.com/zajca/pohunek/issues/2542))
