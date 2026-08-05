# Hermes compatibility evidence

`compatibility-lock.json` is the canonical machine-readable M2 pin: Hermes
Agent `0.20.0`, tag `v2026.8.3`, source commit
`3c27eb6234bf91b8ceee9e9071591b31e9b148cb`, and canonical source-archive
SHA-256 `1e9319c58a7f5e95808546af1091d58472be7437adc63fae0cbb53316e2711aa`.
Runtime code that needs the supported Hermes version should consume this lock
rather than repeat `0.20.0` in another mutable source.

Run the model-free CI gate with an installed pinned executable on `PATH`:

```bash
cargo xtask hermes compatibility
```

For local diagnosis, `--hermes-bin <path>` selects another executable. The
gate creates temporary `HOME`, `HERMES_HOME`, XDG, Python user, bytecode, and
uv-cache locations. It clears the ambient environment except for the minimum
process-launch variables, bounds every command by time and output size, checks
the exact lock digest and source-archive contract, rejects a wrong Hermes
version or CLI shape (including the used profile subcommands and their
positional-argument ordering), and validates every committed golden record. It
never runs a model turn, plugin command, installer, or user profile operation.
Validation does not trust a matching checksum alone: every `captured` fixture
must satisfy the state-specific prompt, response, tool, approval, interruption,
exit, resume, or alternate-screen evidence contract recorded by the refresh.

Refresh PTY evidence explicitly only with operator approval for the potentially
billable real provider turns, naming each provider variable whose existing value
may reach the isolated child:

```bash
cargo xtask hermes refresh-goldens \
  --hermes-bin /absolute/path/to/hermes \
  --provider-env OPENAI_API_KEY
```

Repeat `--provider-env NAME` when the selected provider needs more than one
variable. Names must be in the positive allowlist of credential variables read
by the pinned Hermes provider adapters. Loader, shell, Node, Python, temporary
directory, path-valued credential, `*_FILE`, and `HERMES_TUI` variables are
rejected. Values
are read only for the child environment and never enter argv, output,
diagnostics, manifests, or `Debug`. The refresh clears every other ambient
variable, uses temporary `HOME` and `HERMES_HOME`, and never reads the
operator's Hermes home or `state.db`. Each scenario uses a fixed
repository-owned prompt, bounded semantic readiness/state waits, and bounded
output. Classic captures must prove the expected response or UI state before
they can be marked `captured`: multiline input produces one exact response,
and exactly one pinned Hermes submitted-user boundary; bounded normalization
accepts Rich continuation lines belonging to that single preview,
working and interruption show a running-tool marker, approval shows the real
approval panel, completion yields the exact response and native reference, and
resume restores that exact reference. The harness contains each child in a
process group, terminates the complete group on every exit path, and bounds
pipe/PTY reader shutdown. Before writing, output is
stripped of terminal control sequences, personal and temporary paths, session
identifiers, timestamps, and credential-shaped assignments. Any remaining
secret-shaped output fails the refresh closed.

Prompt readiness, short and multiline turns, working, approval, completion,
interruption, and native resume cannot be regenerated without a provider that
is explicitly available to the isolated child. Resume is derived only from the
fresh isolated capture; no ambient session is continued. The alternate-screen
TUI is attempted separately. Only a recognized, nonempty local Node/workspace/
build-unavailable diagnosis may produce `unsupported`; authentication errors,
crashes, missing alternate-screen evidence, timeouts, and harness failures abort
the refresh. Pending records make `cargo xtask hermes compatibility` fail; review
every refreshed text file before committing it.
