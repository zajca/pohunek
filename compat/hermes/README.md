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

Refresh PTY evidence explicitly with the real pinned Hermes process and PTY
against the repository-owned deterministic model mock:

```bash
cargo xtask hermes refresh-goldens --hermes-bin ABS
```

`ABS` must be the absolute path to the pinned Hermes executable. The harness
still launches that real Hermes binary in a real PTY, but satisfies every
relevant model and remote-metadata dependency locally. Its OpenAI-compatible
endpoint binds only to IPv4 loopback, requires no provider credentials and
incurs no provider cost. Credential-source suppression normally produces no
Copilot startup probe. If the pinned background exchange still starts, the mock
admits at most its three-attempt budget of `CONNECT api.github.com:443`
requests plus three `CONNECT api.githubcopilot.com:443` fallbacks. Fast process
shutdown may shorten those probes or interleave them with scenario traffic. The
mock accepts only the two exact request lines and matching `Host` headers,
returns HTTP 403 before TLS begins, and therefore never receives an
authorization header or token. An over-budget attempt, any other `CONNECT`, any
extra header, or any absolute-form external request fails the scenario. Each of the
six model-bearing classic scenarios must then make this exact localhost
sequence: five ordered detection GETs to `/api/v1/models`,
`/api/tags`, `/v1/props`, `/props`, and `/version`, each receiving a
deterministic HTTP 404; then exactly one `POST /v1/chat/completions`. Discovery
is not cached across those processes. The isolated config statically pins
`pohunek-compat-v1`, `context_length: 64000`, and `discover_models: false`, so
Hermes does not request `/v1/models` and the mock does not permit that path.
Every isolated Hermes home also receives fresh, deterministic
`models_dev_cache.json` and `cache/model_catalog.json` files, while the generated
configuration disables remote model-catalog refreshes. Its isolated `auth.json`
suppresses every Copilot credential source, including the `gh auth token`
fallback. A repository-owned noncredential value is selected before that
subprocess; its pinned three-attempt token exchange is the locally denied probe
described above. An unreachable isolated D-Bus address also prevents child
processes from opening the operator's desktop keyring. Harness-owned HTTP,
HTTPS, and all-protocol proxy
variables point at the loopback mock while `NO_PROXY` is restricted to
localhost. The exact denied Copilot probe is the only admitted non-local proxy
authority; it never opens a tunnel. This is a fail-closed application-level
defense, not OS-level network containment.
The `prompt-ready` and `exit` classic scenarios issue no model API requests.
The mock rejects a wrong order or count and validates the POST model identifier
and last user prompt, plus the terminal tool for terminal scenarios.

The refresh clears the ambient environment except for the minimum
process-launch variables, uses temporary `HOME`, `HERMES_HOME`, XDG, Python,
and cache locations, and never reads the operator's Hermes home or `state.db`.
It sets `HERMES_SKIP_NODE_BOOTSTRAP=1`; the TUI-only process also receives an
empty isolated `PATH`. Missing Node/npm therefore produces reviewable local
`unsupported` evidence instead of installing TUI dependencies or contacting a
package registry. Classic terminal-tool scenarios retain the normal executable
path for their exact repository-owned commands.
Each scenario uses a fixed repository-owned prompt, bounded semantic
readiness/state waits, and bounded output. Classic captures must prove the
expected response or UI state before they can be marked `captured`: multiline
input produces one exact response and exactly one pinned Hermes submitted-user
boundary; bounded normalization accepts Rich continuation lines belonging to
that single preview. Model responses require exactly one ordered terminal
render sequence for the pinned streaming response frame: rounded Hermes header,
exact response content, and rounded footer. The matcher tolerates only the
prompt-toolkit redraws that interleave those real render events and stores a
bounded normalized panel as golden evidence. Working and interruption show a running-tool marker,
approval shows the real approval panel, completion yields the exact response
and native reference, and resume restores that exact reference. The harness
contains each child in a process group, terminates the complete group on every
exit path, and bounds pipe/PTY reader shutdown. Before writing, output is
stripped of terminal control sequences, personal and temporary paths, session
identifiers, timestamps, and credential-shaped assignments. Any remaining
secret-shaped output fails the refresh closed.

Prompt readiness, short and multiline turns, working, approval, completion,
interruption, and native resume are regenerated through the deterministic local
model responses. Resume is derived only from the fresh isolated capture; no
ambient session is continued. The alternate-screen TUI is attempted separately.
Only a recognized, nonempty local Node/workspace/build-unavailable diagnosis
may produce `unsupported`; the harness never bootstraps the missing dependency.
Crashes, rejected model requests, missing alternate-screen evidence, timeouts,
and harness failures abort the refresh. Pending records make `cargo xtask hermes
compatibility` fail; review every refreshed text file before committing it.
