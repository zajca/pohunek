---
type: Guide
id: guide/workflow-plugins
title: Workflow plugins
description: What a workflow plugin is, the workflow.toml manifest, installing a plugin pinned by its digest with pohunek workflow, and the same-UID owner trust boundary.
source_kind: manual
intents: [setup, update, debug, help]
---

# Workflow plugins

A workflow plugin is a bundle of owner tooling, such as a work-overview tool
that joins an issue tracker, a code host and pohunek sessions into one table.
It declares named **actions** as argv arrays and acts on pohunek only through
the public CLI or SDK, like any script the owner runs. It is not a runtime
package: the daemon never loads, lists or runs it, and it cannot extend the
daemon, the session worker, protocol methods, overlay transports or any user
interface.

`pohunek workflow` manages the plugins of the current user on this machine:

```bash
pohunek workflow install work.tar.zst --sha256 <digest>        # review, nothing installed
pohunek workflow install work.tar.zst --sha256 <digest> --yes  # install, enabled
pohunek workflow list [--json]
pohunek workflow inspect acme.work [--json]
pohunek workflow disable acme.work
pohunek workflow enable acme.work
pohunek workflow uninstall acme.work [--purge] [--remove-modified]
```

Running an action, event reactions, periodic jobs, long-running services, and
shell completion of plugin ids are not yet available; the manifest refuses
their tables until they ship (#148, #325, #326).

## The archive and its manifest

A workflow plugin ships as the same canonical `tar.zst` archive as a runtime
package (see the [runtime package archive](../concepts/runtime-package-archive.md)),
with `workflow.toml` at the archive root. An archive that also carries
`runtime.toml` is refused: one archive is never both kinds. In a pohunek
source checkout, `cargo xtask package build <dir> --output <file>` builds the
archive from a directory; executable files keep their executable bit.

```toml
schema = 1

[plugin]
id = "acme.work"            # package id grammar: [a-z0-9._-], no leading . or -, no ..
version = "1.2.0"           # [A-Za-z0-9.+_-], at most 64 bytes
description = "Work overview"

[actions.overview]          # action name: [a-z][a-z0-9_]{0,31}
argv = ["bin/work", "overview"]
args = "passthrough"        # or "none" (the default): extra arguments refused
timeout_ms = 30000          # optional
max_output_bytes = 1048576  # optional
```

Rules, each checked at install time:

- The manifest is at most 64 KiB, checked before it is parsed. Every table
  refuses unknown keys, so `[reactions]`, `[jobs]` and `[services]` are refused
  today. At least one action is required. The description is 1 to 1024 bytes.
- `argv` has 1 to 64 entries of at most 4096 bytes each, with no NUL byte. No
  shell ever interprets it.
- `argv[0]` is either a package path to an executable file of the archive
  (`bin/work`, resolved to the installed copy of that file) or an absolute
  host path such as `/usr/bin/git` (an absolute path must name a file, so `/`
  is refused). A bare name (`work`, `git`) is refused
  even when the archive has a file of that name, because no `PATH` is
  searched. A path with `.`, `..` or empty segments is refused, and so is a
  package path that is missing from the archive or lacks the executable bit.
- `timeout_ms` and `max_output_bytes`, when present, must be positive. They are
  recorded with the plugin; the owner limits they must fit arrive with action
  runs, which are not yet available.

A refused manifest fails with `workflow_descriptor_invalid`. The message names
the broken rule and, where it applies, a line number, the action name (which
already matched the name grammar) or an unsupported schema number. It never
quotes an argv entry, a description or any other manifest string, so an argv
entry cannot leak through an error.

## Install, trust and consent

Trust is the archive digest you supply with `--sha256` (`sha256:<64 hex>` or
the bare hex `sha256sum` prints). There is no catalog for workflow plugins.
Install reads the archive, checks its digest (`workflow_digest_mismatch`
otherwise) and validates the manifest, then prints the id, version, digest,
and every action's argv with `argv[0]` resolved to its absolute path. Nothing
is created until you repeat the command with `--yes`; until then it fails with
`consent_required`. `--no-enable` installs the plugin disabled.

The store keeps one version per plugin id. Installing the recorded archive
again changes nothing (`already_installed`), unless its installed files are
missing: the review says so, and `--yes` extracts them again
(`root_restored`). Another archive of an installed id is refused with
`workflow_plugin_installed` until you uninstall the first. When the store
recorded an install but could not confirm the change is durable, install
succeeds with a warning; when it cannot even read the outcome back, it fails
with `workflow_install_uncertain`, keeps the plugin's directories, and asks you
to check `pohunek workflow list` (installing the same archive again is safe). Every command that
changes the store holds the store lock `<state>/pohunek/workflow-plugins/store.lock`
for its whole run, so two installs never record two versions of one id; a
command that finds the lock held fails with `workflow_store_busy` and changes
nothing.

Where things live, all owner-private (`0700`):

- `<state>/pohunek/workflow-plugins/` holds the plugin registry and the
  verified, content-addressed plugin files. It is separate from the runtime
  package store `<state>/pohunek/plugins/`.
- `<state>/pohunek/workflow-plugin-state/<id>/` is the plugin's state directory.
- `<config>/pohunek/workflow-plugin-config/<id>/` is the plugin's config
  directory.

`<state>` is `$XDG_STATE_HOME` (default `~/.local/state`) and `<config>` is
`$XDG_CONFIG_HOME` (default `~/.config`). The three locations never contain
each other; a configuration that makes one lie inside another is refused with
`workflow_dirs_overlap`. Install requires both parent directories to be
exactly `0700` and fails closed otherwise (`workflow_store_failed`), removing
any plugin directory it created.

The state and config directories are keyed by id, so they survive an
uninstall. `uninstall --purge` deletes them, also for an id an earlier
uninstall already removed. A purge always attempts both directories. When it
cannot finish, for example because a subdirectory is read-only, it fails with
`workflow_purge_incomplete`, says whether the plugin itself was uninstalled,
and names where the remaining data is (a `.purge-<id>+<hex>` directory beside
the plugin's); fix the cause and run the purge again. Every purge first
retries the leftovers in both parents. A leftover of the plugin being purged
is its own data: while it cannot be removed, the purge keeps failing with
`workflow_purge_incomplete` and names where it is. A leftover of another
plugin is retried best effort: one that still cannot be removed is reported in
the result's `warnings` with its location and never decides the outcome.

`inspect` and `enable` re-verify the installed files first and fail with
`workflow_root_invalid` when they are missing or changed since install. The
recovery hint depends on the cause:

- Missing files: install the same archive again, which restores them, or
  remove the plugin with a plain `uninstall`.
- Changed files are left as found: a plain `uninstall` refuses them too, and
  `uninstall --remove-modified` removes them.
- Files that cannot be read: nothing is judged or removed; fix the store's
  ownership or permissions and retry. When reading was refused for lack of
  permission, `uninstall --remove-modified` also removes the files.

`--remove-modified` refuses files that verify or are missing
(`workflow_root_intact`). When an uninstall removed the record but some
installed files could not be deleted, the command succeeds with a warning that
names the `.collect-*` residue below `<state>/pohunek/workflow-plugins/packages/`;
later store changes retry removing it, best effort.

## Local only

Every `pohunek workflow` command works on this user's files and never contacts
a daemon. A remote `--host` fails with `workflow_local_only` before any file is
read or written.

## Trust boundary

A workflow plugin runs with your account's full authority. The digest proves
which bytes you approved; it does not restrict what they do. Plugin code can
read your files and use every public owner API, including the CLI and the SDK,
as any same-user process can. That is the same owner trust boundary as a
third-party runtime package, not a sandbox. Install only plugins whose source
you trust, and review the argv the consent step prints.

Error codes: `workflow_local_only`, `workflow_archive_invalid`,
`workflow_digest_mismatch`, `workflow_descriptor_invalid`, `consent_required`,
`workflow_plugin_unknown`, `workflow_plugin_installed`,
`workflow_root_invalid`, `workflow_root_intact`, `workflow_install_uncertain`,
`workflow_store_busy`,
`workflow_purge_incomplete`, `workflow_dirs_overlap`, `workflow_store_failed`.
