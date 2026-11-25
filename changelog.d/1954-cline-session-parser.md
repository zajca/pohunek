---
kind: added
---
A runtime package can attribute a foreground Cline session through its session store: the new runtime feature `transcripts-cline-session-v1` provides the parser `cline-session-v1`, which reads `<id>/<id>.json` below the `[transcripts] dir` (`data/sessions` of the Cline config home) and takes the session id and the working directory from the manifest. Subagent sessions and `*.messages.json` files are ignored, and a manifest over 64 KiB is not read. A profile that sets only `CLINE_DATA_DIR` is not followed. ([#1954](https://github.com/zajca/pohunek/issues/1954))
