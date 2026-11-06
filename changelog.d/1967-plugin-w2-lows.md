---
kind: fixed
---
`pohunek plugin install` by package id follows a chain of ten redirects instead of refusing the tenth hop. A failed or oversized download of the plugin index or an archive (`plugin_index_unavailable`, `plugin_archive_unavailable`, `plugin_download_too_large`) now carries the `transport` error class instead of `configuration`.
