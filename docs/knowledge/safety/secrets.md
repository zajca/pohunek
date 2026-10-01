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

The Linear provider library in `pohunek-gui-core` reads its API token from the
platform credential store (macOS Keychain, Linux Secret Service) by entry name.
The Linear client is not yet wired into the GUI, so no GUI surface shows these
errors today; the typed errors are a library contract for the code that will.
The config holds only the entry name; the value is read afresh for every
request and is never cached, logged, put in an error, or written to config or
state. There is no in-memory or file fallback: without a credential store the
lookup fails.

A failed lookup carries a `TokenErrorKind`, and its message names the entry (a
reference), never the value:

- `NotFound`: no entry with that name; add one under the configured service.
- `Locked`: macOS only. The keychain is locked and no unlock UI may be shown
  (`errSecInteractionNotAllowed`) or the user dismissed the prompt. Keyring
  never reports `Locked` on Linux: a locked Secret Service collection surfaces
  as `Unavailable`.
- `Unavailable`: no keychain or Secret Service provider is reachable, or the
  store refused access to the entry (`errSecAuthFailed`).
- `Timeout`: see below.
- `Invalid`: the entry exists but cannot be used, such as a non-UTF-8 value.
- `Other`: a failure with no more specific kind, such as a lookup task that
  panicked; the message is fixed and never carries the panic payload.

`LinearError::token_error_kind()` gives one contract for both shapes of
credential failure: the caller-side lookup timeout and a store-reported error
both map to a `TokenErrorKind`, the timeout to `Timeout`.

### Blocking and timeouts

A keychain read cannot be cancelled and may wait on an unlock prompt
indefinitely. The caller's `token_lookup_timeout` bounds the wait, not the
blocking thread. In an interactive GUI session a locked keychain shows the
unlock prompt, so the lookup surfaces as `Timeout` (the caller gave up) and
later lookups of any key are refused fast as `Timeout` while that read is
still pending. A locked keychain blocks every entry, so one lookup runs at a
time for the whole store; repeated attempts cannot pile up blocking threads.
Only a non-interactive process sees `Locked`.

`gh` (GitHub provider) authenticates through its own credential handling and
does not use the credential store.

### Keychain test policy

Tests never read or write the operator's login keychain. The real-backend macOS
test (`crates/gui-core/tests/keychain_macos.rs`) requires
`GUI_CORE_TEST_KEYCHAIN` to name a keychain whose file name contains the
`KEYCHAIN_MARKER` constant (defined only in that file; CI reads it from there),
that lives outside `~/Library/Keychains` and `/Library/Keychains`, and that is
already the user-domain default. It checks all of this before its first write
and refuses otherwise. CI creates the keychain with a random, masked,
step-local password and makes it the only keychain in the user search list. An
always-run teardown deletes it, restores the original list and default, and
fails the job if they differ afterwards. Locally the test prints a `SKIPPED`
line when the variable is unset; on CI a missing variable fails the test.

The real keychain covers: entry not found, success, locked (with user
interaction disabled, so it reports `Locked` rather than prompting), and the
keychain file gone (`Unavailable`). Access denial (`errSecAuthFailed`), the
bounded wait, the interactive unlock prompt, and the one-lookup-at-a-time guard
are covered only by unit tests over the status-code classification and
injected lookup closures.
