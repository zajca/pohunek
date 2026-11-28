---
kind: fixed
---
The TypeScript SDK (`@pohunek/sdk`) now walks `session.list` and `project.list` pages itself, like the Rust SDK: it follows `cursor` with pages of 200 records, halves the page on a `session_list_too_large` / `project_list_too_large` reply, recognizes a daemon that predates paging, and bounds the walk with the client-raised `session_list_cursor_repeated` / `project_list_cursor_repeated` and `session_list_walk_limit` / `project_list_walk_limit` errors. An explicit `limit` still sends one request. ([#2570](https://github.com/zajca/pohunek/issues/2570))
