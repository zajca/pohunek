## Unreleased (2026-11-08, detection status, #2044)

- `session.detection` gains an optional additive `status` object and `pohunek session detection` prints it before the region previews: the `state_source` that decided the activity, whether the OSC 7501 probe is answered, the held record and its state, the release gate (`declared`, `open`, `reason`, `runtime_version`, `authority_from`) and the age of the last applied evidence. A probe the worker does not answer under a closed gate reads `unanswered, screen only`. See `docs/public-api.md` (`session.detection`).
