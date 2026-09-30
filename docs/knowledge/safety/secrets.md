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

Host governance has the same split. A safe `host.governance.inspect` response
may contain a stable host ID and public approval-key reference, but it must
never contain the private approval signing key or seed, a transfer proposal or
outcome, a nonce, a signature, a retired-enrollment record, or relay
credentials. Treat all of those values and the owner-private host-state files
as secret-bearing operational material even when their filenames are known.

## Provider tokens in the platform credential store

The GUI reads the Linear API token from the platform credential store (macOS
Keychain, Linux Secret Service) by entry name. The config holds only the entry
name; the value is read afresh for every request and is never cached, logged,
put in an error, or written to config or state. There is no in-memory or file
fallback: without a credential store the lookup fails.

Lookup failures are typed so the GUI can say what to do next, and messages name
the entry (a reference), never the value:

- `NotFound`: no entry with that name; add one under the configured service.
- `Locked`: the store is locked; unlock the login keychain and retry. On macOS
  this is `errSecInteractionNotAllowed` or a dismissed unlock prompt.
- `Unavailable`: no keychain or no Secret Service provider is reachable.
- `Timeout`: the store did not answer within the token lookup timeout, or an
  earlier lookup of the same entry is still stuck. A locked macOS keychain may
  wait on an unlock prompt indefinitely, so the timeout bounds the caller; only
  one blocking lookup per entry runs at a time, so repeated attempts do not
  pile up threads.
- `Invalid`: the entry exists but cannot be used, such as a non-UTF-8 value.

`gh` (GitHub provider) authenticates through its own credential handling and
does not use the credential store.

### Keychain test policy

Tests never read or write the operator's login keychain. The real-backend macOS
test (`crates/gui-core/tests/keychain_macos.rs`) requires
`GUI_CORE_TEST_KEYCHAIN` to name a throwaway keychain that is already the
user-domain default, and refuses to run otherwise. CI creates that keychain with
a random, masked, step-local password, makes it the only keychain in the user
search list, and restores the original list and default in an always-run
teardown step. Locally the test prints a `SKIPPED` line when the variable is
unset; on CI a missing variable fails the test. States a real keychain cannot
produce reliably are covered by unit tests over the status-code classification
and an injected lookup backend.
