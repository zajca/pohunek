# GUI Assistant Launch Design

## Goal

Add a native GUI entry point for launching the Universal Pohunek Assistant as a
normal PTY-backed session.

## Scope

The GUI shows an `Assistant` button above the workspace tree. The button opens a
modal that lets the operator choose both the assistant intent and the agent
runtime/profile, enter a free-form request, and start a session in the selected
project context.

The launch must reuse the same assistant behavior as the CLI:

- collect or intentionally skip the redacted snapshot;
- materialize the knowledge bundle on the host that will run the agent;
- preflight bundle/snapshot read access;
- compose the navigational opening prompt;
- call `session.new` with that prompt as initial input.

## Decisions

1. Use a shared library path rather than shelling out to `pohunek assistant`.
   This keeps the GUI native, avoids parsing CLI output, and keeps CLI/GUI
   behavior aligned.
2. Keep the daemon protocol unchanged. The assistant remains ordinary client-side
   orchestration over existing `host.inspect`, `assistant.materialize`, and
   `session.new` methods.
3. Launch against the selected project. If a session is selected, use its bound
   project. If no project context is available, the GUI reports a clear error
   before launch.
4. The agent picker includes `Auto` plus the available non-shell runtimes from
   `host.inspect`. `Auto` preserves the CLI ranking: `pohunek-assistant`,
   `codex`, `claude`, then another available non-shell runtime.
5. The modal includes an Advanced section for branch/base branch and explicit
   snapshot options. Full mode is the default; degraded mode stays an explicit
   opt-in.

## UI Shape

The workspace panel starts with a compact icon-and-text `Assistant` button above
the tree. The `Start assistant` modal shows:

- selected host and project context;
- `Intent` pick list: `help`, `setup`, `project`, `update`, `debug`;
- `Agent` pick list: `Auto` and host runtime/profile names;
- request editor;
- Advanced controls for branch, base branch, `No snapshot`, and `Degraded`;
- primary `Start assistant` action.

On success, the GUI applies the created session to workspace state and uses the
existing attach command flow to open it in a terminal.

## Error Handling

Errors are surfaced in the GUI status/toast path. The launch must fail closed for
missing project context, missing capable agents, materialization failures,
readability failures, unsupported remote degraded launches, and `session.new`
failures. It must not silently fall back to a weaker launch.

## Tests

New behavior is covered with test-first changes:

- shared assistant selection/target validation tests;
- assistant prompt launch parameter tests;
- GUI helper tests for resolving selected project context;
- focused view/update tests where practical;
- existing CLI assistant parser and prompt tests remain valid.

## Knowledge Updates

The assistant knowledge bundle documents GUI behavior, so `docs/knowledge` must
be updated in the same change. The GUI guide and assistant source map must point
to the new shared launcher and GUI entry point.
