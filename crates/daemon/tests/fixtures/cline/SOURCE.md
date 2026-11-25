# Cline session manifest fixture

`session-manifest.json` is the session manifest Cline 3.0.74 writes at
`<data>/sessions/<id>/<id>.json`, captured by the model-free stub run in
`pohunekd/plugin-cline` (`compat/sessions/assigned-first-turn/manifests.json`, its
only element). The scenarios that read it replace three placeholders:

- `@SESSION@` becomes the session id (`session_id`, and the directory it lies in);
- `@WORKSPACE@` becomes the working directory (`cwd`, `workspace_root` and the
  `messages_path` prefix);
- `@PROMPT@` becomes the prompt, which a scenario pads to put the manifest at or
  over the read bound.

The capture holds no host path, user name or key; every other value is as
captured.
