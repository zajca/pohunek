---
kind: fixed
---
`project.list` accepts `limit` and `cursor`, and a project set that does not fit one control line is answered with the typed `project_list_too_large` error instead of a frame the client refuses. `pohunek project list` and the Rust SDK `Client::project_list` page through the whole set themselves. ([#1110](https://github.com/zajca/pohunek/issues/1110))
