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

Inside the daemon the profile environment travels in the `LaunchEnv` carrier,
whose `Debug` form prints only an entry count, and in `ResolvedProfile`,
`LaunchOpts` and `LaunchCommand`, which embed it; a stray `{:?}` of any of them
cannot print a name or a value. The store keeps only the keyed revision (a MAC)
of the profile a session launched under, never its env, and error messages of
the profile-change refusals name the agent and session, never a profile value.

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

## Config homes derived from profile environments

`pohunek integration install|status|doctor|uninstall` with `--profile` or
`--all-profiles` act on directories a host profile's `[env]` names. Their
reports carry paths and errors derived from those values, so the daemon serves
the selectors on the local control socket only and refuses a remote connection
before acting. Typed errors name the variable that supplied a refused value,
never the value. A profile value that is not an absolute path is refused, not
expanded against the daemon's home directory.

## Provider tokens

The daemon and the CLI never hold Linear or GitHub provider tokens. Provider
integrations live in client surfaces: `gh` authenticates through its own
credential handling, and a client that reads a Linear token takes it from the
platform credential store (macOS Keychain, Linux Secret Service) by entry name,
reading it afresh for every request and never caching it, logging it, putting it
in an error, or writing it to config, session metadata, daemon state, or the
event log. Relay credentials the CLI stores go through the same platform
credential store and follow the same rule: references only, never values.

On macOS the release binaries are ad-hoc signed, which gives them no stable
designated requirement. After an upgrade the system may therefore ask again
whether `pohunek` may access its `pohunek-relay` Keychain items; "Always Allow"
applies to that build only. This is expected, not a sign of tampering.
