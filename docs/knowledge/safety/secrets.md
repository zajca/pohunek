---
type: SafetyPolicy
id: safety/secrets
title: Secrets and public-safe knowledge
description: The committed and materialized knowledge bundle must never contain secret values, and snapshots must be built from an explicit allowlist.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Secrets and Public-Safe Knowledge

The knowledge bundle is public-safe. Manual concepts, generated reference,
runbooks, prompt templates, and source maps must not contain secret values,
credentials, tokens, private keys, or environment-specific secret data.

Profile `[env]` entries are secret-bearing even when their names look harmless.
Do not copy profile environment keys or values into prompts, snapshots,
documentation, logs, commits, or issue text.

The assistant snapshot is allowlist-built. It may include filenames, existence
status, parse status, selected action names, structured command output, and
warnings. It must not collect process environment variables, profile env values,
hook script bodies, arbitrary config bodies, or credentials embedded in URLs.

When a task requires editing secret-bearing config, explain the file and field to
the user, but leave the value out of the response. Store secrets only through the
project's established mechanism.
