## Unreleased (2026-10-13, workflow run --json and verbatim arguments, #325)

- `pohunek workflow run [--json] <plugin> <action> [<args>...]`: every
  argument after `<action>` reaches the action verbatim, including `--json`,
  `--`, `-h`, `--host` and empty strings, so pohunek's own options go before
  `<plugin>`. A `--` after the action is an argument for the action.
- `--json` captures the action's stdout up to its cap and prints one CLI
  envelope whose `ok` is the action's single JSON document byte for byte;
  stderr still streams. The context's `output` is `json`.
- New error codes, each an envelope without output bytes:
  `workflow_action_failed` (non-zero exit or a signal the CLI did not send;
  carries `exit_code` and `signal?`, and the CLI exits with the action's
  status), `workflow_output_invalid` (exit 0 but stdout is not exactly one
  JSON document, including arrays and objects nested deeper than 126 levels;
  exit 1) and `workflow_output_incomplete` (exit 0 but an
  escaped process held stdout open; exit 1). The run log gains the outcomes
  `output_invalid` and `output_incomplete`.
- The workflow plugins guide, `docs/public-api.md` and `docs/cli.md` describe
  it.
