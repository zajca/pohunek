## Unreleased (2026-11-15, plugin prove for context entries, #2170)

- `cargo xtask plugin prove` proves a `kind = "context"` index entry the way it
  proves a runtime one: the throwaway trust signs a schema 2 index
  (`index throwaway --schema-version 2`, `official-index-v2.json`), the digest
  step refuses a descriptor that carries a `[runtime]` table or no `[context]`
  table, the install must report the context package enabled and selected, and
  the inspect reads the package back (`pohunek.context.assistant` also renders
  the assistant prompt through a stub agent the rig writes before the daemon
  starts, so the readback touches the installed package; `assistant help
  --agent prove-assistant --print-prompt --json` starts no session, so the stub
  never launches). The evidence carries `"kind": "context"`. The proof
  daemon always starts with `--local-only` (#2372), and so does the isolated
  Hermes package host and the release smoke ([Build and prove a plugin archive](runbooks/plugin-index-keys.md#6-build-and-prove-a-plugin-archive),
  [Plugin repositories](guides/plugin-repositories.md)).
