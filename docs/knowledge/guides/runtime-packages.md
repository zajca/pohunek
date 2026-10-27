---
type: Guide
id: guide/runtime-packages
title: Runtime packages
description: Install, inspect, update, select, disable, and remove runtime packages with pohunek plugin, including the trust model, the consent step, and what disable and uninstall do to sessions.
source_kind: manual
intents: [setup, update, debug, help]
---

# Runtime packages

A runtime package adds an agent runtime (a program the daemon launches inside a
session) to the host. `pohunek plugin` manages the packages installed on the
machine the daemon runs on. See the
[runtime package archive](../concepts/runtime-package-archive.md) for the file
format and the [runtime catalog](../concepts/runtime-catalog.md) for official
trust. The official packages are [Pi](pi-package.md), [Codex](codex-package.md),
[Claude Code](claude-package.md) and [Hermes](hermes-package.md). Every Linux
daemon archive of a release carries them under `runtime/packages/`, with the
signed `runtime/runtime-catalog.json` that authorizes them (see
[Install official packages from a release](#install-official-packages-from-a-release)).
The Codex, Claude Code and Hermes packages serve the reserved `codex`, `claude`
and `hermes` ids only through that catalog. The daemon compiles none of Codex,
Claude Code and Hermes in: `codex`, `claude` and `hermes` are each served by
the official package or by nothing; `shell` is the only built-in runtime. Recovery steps for verification, trust and
profile pin failures are in the
[runtime package recovery runbook](../runbooks/runtime-package-recovery.md).

```bash
pohunek plugin list
pohunek plugin install ./acme-pi-1.0.0.tar.zst --sha256 sha256:<64 hex>
pohunek plugin install ./acme-pi-1.0.0.tar.zst --sha256 sha256:<64 hex> --yes
pohunek plugin inspect acme.pi
pohunek plugin update acme.pi ./acme-pi-1.1.0.tar.zst --sha256 sha256:<64 hex> --yes
pohunek plugin select acme.pi --version 1.0.0
pohunek plugin disable acme.pi --version 1.1.0
pohunek plugin uninstall acme.pi --version 1.1.0 --yes
pohunek plugin doctor
```

Every subcommand accepts `--json`: one envelope on stdout, digests in full.
Tables abbreviate a digest to its first 12 hex characters; that prefix is a
valid `--digest` selector.

## Install official packages from a release

Install from the extracted daemon archive of the release the daemon runs. The
catalog pins every entry to exactly that core version (`core = "=<version>"`),
so a catalog of another release is refused with `package_incompatible` (or
`package_untrusted` when it is older than a catalog the host already accepted):

```bash
cd pohunek-daemon-<version>-<target>
pohunek plugin install runtime/packages/pi.tar.zst --catalog runtime/runtime-catalog.json
pohunek plugin install runtime/packages/pi.tar.zst --catalog runtime/runtime-catalog.json --yes
```

Current releases list the Linux x86_64 glibc and musl targets, and Apple Silicon
(aarch64-apple-darwin). `pohunek doctor` must report the
`catalog_trust_anchor` check as `ok` first. Installing `codex.tar.zst`,
`claude.tar.zst` or `hermes.tar.zst` makes the package the only source of that
runtime (see [Official aliases](#official-aliases)); install the Codex or
Claude hooks afterwards with `pohunek integration install --agent codex` or
`--agent claude`, and the Hermes operator plugin as the
[Hermes operator](hermes-operator.md) guide describes.
`--no-enable` stages a package until `plugin enable`. `docs/install.md` has the complete procedure.
A `pohunek service upgrade` runs this install for the release's packages it needs
and pins the host profiles to them, so the manual install is the fallback for a
host that upgrades without that step.

## Runtime features

`runtime_api` stays the major number of the descriptor contract. A capability
added to the core within a major number is a named runtime feature, with an id
shaped `<name>-v<N>` such as `input-readiness-v2`. A descriptor that uses a
feature lists it in a top-level `requires` array, before the first table:

```toml
runtime_api = 1
requires = ["input-readiness-v2"]
```

`requires` needs core 0.38 or later: an older core does not know the key and
faults the package as `descriptor_invalid`. It holds at most 16 unique feature
ids. The core checks it before the
rest of the descriptor:

- An id this core does not implement is refused with
  `runtime_feature_unsupported`. The daemon log names the feature and the
  core's version; the wire message is fixed text, like every other
  package error. The refusal wins over `descriptor_invalid` for a key only
  that feature's core understands. An installed package that fails this check at
  load is left out and faulted as `descriptor_invalid`; other packages and
  built-ins still load.
- A descriptor key, or a compiled parser or handler id, that belongs to a
  feature is accepted only when `requires` lists that feature. Using it
  without the entry is refused with `runtime_feature_unsupported`, and the
  log names the key path.
- A `requires` entry the descriptor does not use is accepted.
- An entry that is not a feature id, a repeated entry or more than 16 entries
  is `descriptor_invalid`.

The features this core implements:

| Feature | Since | Gated descriptor key or value |
|---|---|---|
| `config-home-subdir-v1` | 0.38.0 | `[config_home] subdir` |
| `version-probe-revision-v1` | 0.38.0 | `version_probe.parser = "semver-revision-line-v1"` |
| `launch-env-v1` | 0.38.0 | `[launch_env]` |
| `transcripts-data-root-v1` | 0.38.0 | `[transcripts] root_env`, `root_home`, `max_depth`; `parser = "muse-session-v1"`; `[native_reference.existence] dir_name` |

The core owns an append-only table of its features. A row is never removed or
renamed, because a published descriptor may require it; the table equals a
snapshot file that a test compares. Which core version first has a feature is
not part of the refusal, because an older core cannot know it.

## Config home

A runtime that keeps its settings, hook registration or conversations in a
directory of its own declares it in an optional `[config_home]` table, separate
from `[integration]` because the home concerns every runtime:

```toml
[config_home]
env = "ACME_HOME"          # variable the agent reads to relocate its home
default = ".config/acme"   # directory below the user's home used otherwise
```

`env` is an upper-case variable name outside the reserved `POHUNEK_` namespace;
`default` is a relative path of plain components (no `..`, no absolute path, no
glob or shell character, bounded in length and depth). Both keys are required
and unknown keys are refused (`descriptor_invalid`) before anything is recorded.
The daemon resolves the home the way a launch does (the variable from the
environment the launched agent sees, a host profile's `[env]` overriding the
daemon's base environment, else the default below that environment's `HOME`) and
never expands a `~`. A runtime that names a daemon-run integration handler
needs the table, or `integration install` answers `agent_config_home_undeclared`
for it. A runtime without an integration (Pi) may omit it; Pi keeps declaring
its conversation root under `[native_reference.existence]`.

A runtime whose variable holds a parent of its home declares `subdir`
(`requires = ["config-home-subdir-v1"]`): with `env = "XDG_CONFIG_HOME"`,
`subdir = "muse"` and `default = ".config/muse"` the home is
`$XDG_CONFIG_HOME/muse` while the variable is set and `~/.config/muse`
otherwise. `subdir` joins the variable's value only; `default` stays the
complete path. A relative or non-UTF-8 value is an error and never falls back
to `default`, unlike the XDG specification. Two runtimes are ambiguous owners of
a home by its resolved path, so the same variable with different `subdir` values
is not.

## Launch environment

A descriptor sets flag environment variables for its launches and its
version probe with `[launch_env]` (`requires = ["launch-env-v1"]`):

```toml
requires = ["launch-env-v1"]

[launch_env]
ACME_NO_AUTO_UPDATE = "1"        # in a runtime with id `acme`; stops a self-updating launcher
```

Use it for behaviour flags the agent reads at start. At most 16 entries; a value
is exactly `0`, `1`, `true` or `false`, never expanded (widening the set needs a
new feature id). A name must start with the runtime's own
namespace: the `runtime.id` in upper case with `-` and `.` as `_`, then `_`
(`muse` gives `MUSE_`, so `MUSE_NO_AUTO_UPDATE`), with something after it. The
core allowlist outside the namespace is empty. A runtime whose namespace
collides with a variable family of the loader or the owner's tools (`SSH_`,
`AWS_`, `GIT_`, `HTTPS_`, `NO_`, `XDG_`, `POHUNEK_`, interpreter, package-manager,
privilege and network tools such as `BASH_`, `NODE_`, `PYTHON`, `SUDO_` and
`CURL_`, and the rest listed in `docs/public-api.md`) has no usable table. Inside the namespace, service-manager
variables, the variable the descriptor reads its config home from, secret-shaped
names (a `KEY`, `KEYS`, `TOKEN`, `TOKENS`, `SECRET`, `SECRETS`, `PASSWORD`, `PASSWD`,
`PASSPHRASE`, `PASSPHRASES`, `CREDENTIAL`, `CREDENTIALS` or `AUTH` word in any
position) and location, identity or code-shaped names (any `_`-separated word after the
namespace in the list in `docs/public-api.md`: `PATH`, `DIR`, `HOME`, `ROOT`,
`FILE`, `URL`, `HOST`, `PROXY`, `CMD`, `BIN`, `OPTS`, `ARGS`, `FLAGS`, `EXEC`,
`SHELL`, `EDITOR`, `HOOK`, `PLUGIN`, `LIB`, `PRELOAD`, `CERT`, `CA`, `KEYRING`,
`CONFIG`, `ID`, ...; a second layer behind the value rule, not a complete
vocabulary) are
refused at install (`descriptor_invalid`). A host profile's `[env]` entry of the same name overrides a declared
pair on a launch. A program the user starts by hand inside a shell session
gets no descriptor environment, so it keeps its own defaults.

## Transcripts

A runtime whose agent writes JSON Lines conversation transcripts below its
config home may declare them in an optional `[transcripts]` table, so external
observation (`POHUNEK_OBSERVE_EXTERNAL_AGENTS=1`) can enrich agents started
outside pohunek:

```toml
[transcripts]
parser = "jsonl-head-v1"   # compiled parser core provides
dir = "sessions"           # transcript tree below [config_home]
```

`parser` names a parser compiled into core: `jsonl-head-v1`, or `muse-session-v1` (below). It
reads at most the first 32 records (64 KiB) of each `*.jsonl` file and takes the
first `session_id`, `sessionId`, `conversation_id` or `conversationId` (in that
order within the first record that has one), `transcript_path` or
`transcriptPath`, and an absolute `cwd`, `working_directory` or
`workingDirectory`. `dir` is a relative path of plain components (no `..`, no
`.`, no empty segment, no absolute path, no glob or shell character, at most
128 bytes and 8 components). The table requires `[config_home]`; an unknown
parser id, an unsafe `dir`, a missing key, an unknown key or a table without
`[config_home]` is refused (`descriptor_invalid`) before anything is recorded.
A runtime without the table (Pi) is served as usual; its config home
contributes no transcript root. When the transcript trees of two different
runtimes are the same directory or one lies inside the other (a shared config
home with the same or a nested `dir`, or a symlink into another runtime's
tree), the observer attributes both trees to neither and logs a warning
`external.transcript_roots.ambiguous` with code `transcripts_ambiguous_owner`
for each home; the other roots stay watched. The official `codex` package
declares `sessions` and the official `claude` package declares `projects`.

### Transcripts outside the config home

An agent that keeps its session logs elsewhere, under `XDG_DATA_HOME` for
instance, names the root itself (runtime feature `transcripts-data-root-v1`,
listed in `requires`) and bounds the walk of a deep tree:

```toml
requires = ["transcripts-data-root-v1"]

[transcripts]
parser = "muse-session-v1"
root_env = "XDG_DATA_HOME"   # variable that relocates the root
root_home = ".local/share"   # root below the user's home while it is unset
dir = "muse/sessions"        # log tree below the root
max_depth = 4                # directory levels below dir the observer lists
```

`root_env` and `root_home` come together and resolve like `env` and `default`
of a `[config_home]` (a host profile's `[env]` over the daemon's base
environment, else the default below `HOME`; a relative value is a typed error,
never a fallback). With them the table needs no `[config_home]`, and the
variable cannot be a `[launch_env]` name. `max_depth` is 1 to 6; the observer
lists no directory below it and registers no watch there.

`muse-session-v1` reads a session log at exactly
`YYYY/MM/DD/<uuid>/session.jsonl` below `dir`. A line is a framed transaction
(`children[].record_json`) or a bare record; within the first 32 lines (64 KiB)
the first record whose `payload_type` is `runtime.session.metadata` gives the
session id (`stream.id`, which must equal the `<uuid>` directory name) and the
working directory (`payload.record.workspace_root`, used only when absolute).
A log nested below a session (a subagent log), at another depth or in a
directory that is not a UUID is not a session. The walk visits the highest
directory names first, so a tree that outgrows the per-pass entry bound has its
newest sessions indexed. A foreground agent started inside a shell session is
attributed through this root; the agent's session log is the only evidence.

A hook runtime whose conversation is a directory named after its id declares
that next to its `hook` strategy:

```toml
[native_reference.existence]
check = "file"
root_env = "XDG_DATA_HOME"
root_home = ".local/share"
dir = "muse/sessions"
dir_name = "{reference}"
file_name = "session.jsonl"   # a plain name, no placeholder
max_depth = 3                 # levels the scan descends below dir (YYYY/MM/DD)
```

Recovery refuses (`agent_native_reference_missing`) a reference without a
regular `<dir_name>/<file_name>` there. The scan never follows a symlink, goes
through the highest names first and is bounded like the other existence checks.
`dir_name` must be `{reference}`; it is not accepted with `strategy =
"assigned"`, `name_match`, `store_env`, `store_max_depth` or `relocating_*`.

## Hook schemas and integration handlers

A package whose agent reports lifecycle hooks names its compiled integration
handler and the hook schema its reports follow in the descriptor:

```toml
[integration]
handler = "codex-hook-v1"
hook_schema = "identity-subagent-v1"
```

Core owns the schemas: `identity-v1` (identity, release, notification; only the
PTY root itself, reporting in process while it runs the frozen launch
entrypoint, may set or update the launch identity) and
`identity-subagent-v1` (the same plus subagent start and stop, the subagent
snapshot and, for Claude only, the prompt-accepted report; the launch identity
comes from the earliest process named for the provider, or running a native-installer release image `<provider>/versions/<release>`). Under
`identity-subagent-v1` a hook, which runs as a descendant of the launch
process, files that process's report, so any process the launch process
starts can file it: the agent's process tree runs as the owner and is not an
identity boundary. On the worker path only `identity-v1` binds the root's
report to the root's own kernel peer; the public `session.report_native_id`
fallback admits only the PTY root's own process and refuses `identity-v1`
sessions. A package only
names one; it cannot carry schema contents, add actions, or relax validation.
The `identity-v1` launch rule requires the entrypoint's shebang interpreter to
run the script itself. An interpreter that re-executes another image, such as a macOS
framework Python stub or an `xcrun` shim like `/usr/bin/python3`, leaves the
session without a launch identity: every claim is refused as
`image_mismatch` or `cmdline_mismatch` (a `worker.launch_identity.rejected`
warning in the worker log). The runtime still runs, but native resume is
unavailable for that session.
`plugin install` and `plugin install --yes` refuse a descriptor whose schema id
or handler id core does not provide, or whose handler does not drive the schema
(`descriptor_invalid`), before anything is recorded. A package without
`[integration]` (Pi) has no schema, and its sessions accept no hook report.
The worker orders identity claims and releases per reported process (pid and
kernel start identity): a claim or release must carry a sequence newer than
every claim and release the worker accepted for that same process. Reports of
different processes are never compared, so a nested agent's claim, even at
`u64::MAX`, never makes the launch agent's later claim stale; the latest
accepted claim is the active identity. A release answered `ok: false` because
its process is not active still orders that process's later claims. The worker
keeps every live reporter's sequence and never evicts it; when live reporting
processes other than the PTY root and the launch process fill its bound (16), a
further process's report is refused with reason
`identity_reporters_exhausted` until one of them exits (an exited process its
parent has not reaped no longer counts). The launch process's own report with a
native reference is verified as a launch claim even at the bound, and kept when
it establishes the launch identity (a deferred verification keeps only the
pending launch claim, which the retry can still establish). A claim's sequence must therefore be
strictly increasing across every hook event that reports the same process. The
core Codex and Claude hooks run a short-lived interpreter per event, so they
read the host's system-wide monotonic clock in nanoseconds (`CLOCK_MONOTONIC`)
with Python's `time.clock_gettime_ns(time.CLOCK_MONOTONIC)`, because
`time.monotonic_ns()` is defined only within one process (Apple's Python 3.9
on macOS counts it from interpreter start). The Hermes reporter stamps its
claims in its long-lived plugin process from `time.monotonic_ns()`, kept
strictly increasing by a counter.
A package whose reference is `assigned` and that also declares an integration can
report its conversation: a validated report replaces the assigned reference and
labels it `reported` (a `/clear` or in-session resume is followed), while a
package without an integration keeps its `assigned` reference.

The handler is compiled core code selected by the descriptor's
`integration.handler`: `pohunek integration install`, `status`, `doctor`, and
`uninstall` resolve the runtime definition and run the handler it names, never a
branch on the runtime id. A handler owns exactly one active asset set, the paths
and modes it may write, conflict detection, rollback, and the commands it may
run; a package cannot ship shell installers, JSON patch programs, or filesystem
targets. Re-installing is an update transaction: the handler stages the new
assets against the active ones without changing them, checks that the hook
schema admits everything the new set reports, derived from the staged
registration and the embedded scripts' action tables rather than from a
declaration (`integration_update_incompatible` otherwise), activates atomically, and
restores the exact prior tree if any step fails, so the old set stays active
until activation succeeds. The check covers the hook schema of every package
version that a live, lost or resumable session or a host profile pin still
references for that handler, not only the schema of the selected version, and an
update of any handler is refused while such a version cannot be read.
The reporter scripts a handler installs, the Claude and the Codex ones alike,
are core-owned templates: the handler
renders them from the runtime's id and display name before staging, a value
outside a conservative character set is refused with a typed error, and the
rendered bytes are what is installed and what drift is compared against. A
package never supplies script bytes.
`hermes-hook-v1` is registered but its lifecycle runs
in the CLI; the daemon answers `agent_not_installable` for it.
The `--agent` flag of `pohunek integration` takes the runtime id of a package runtime that
names a daemon-run handler, and the recovery commands in status and doctor name
that id. Several runtimes may name one handler; a home that several of them reach
through that handler (a package declaring `claude-hook-v1` with Claude's
`[config_home]`, say) is attributed to none of them by an `integration`
request that names no runtime: each reports `integration_home_ambiguous_owner`
for it (the doctor as a `config_root_invalid` finding naming `--agent`), a bare
`install` fails before writing any home, and `--agent <runtime>` addresses one
of them explicitly. A bare `install` on a host where no served runtime has
daemon-managed hooks fails with `integration_no_managed_runtime`, whose hint
names the official package install and then `--agent <runtime>`.
`--all-profiles` covers the host profiles over a runtime that do not resolve as
well: status, doctor and uninstall reach the home their own `[env]` names, and
install lists them as failing.

## Local only

Package commands act on the daemon on this machine through its local control
socket. A remote `--host` is rejected before anything runs (code
`plugin_local_only`), and the daemon refuses package methods on remote overlay
connections too. Installing a package extends what the owner's account may
launch on that machine, so it is never a remote operation.

## Response budget

`install` and `update` wait up to 30 s for one daemon response, where the other
commands use the SDK's 5 s default: validating and unpacking a whole archive
can take longer than 5 s on a slow host or for a large archive. The budget is
the CLI's own tolerance for a slower install, not a daemon install deadline —
the daemon applies no per-install wall-clock deadline of its own. A command
that hits the budget fails with `request_timeout` while the daemon may still
have installed or updated server-side, so check `pohunek plugin list` before
repeating the command; the CLI never retries an install on its own.

## Trust model

- `--sha256 <digest>` authorizes a third-party archive by the digest the owner
  supplies (`sha256:<64 hex>` or the bare hex `sha256sum` prints). The archive
  carries no signature; the owner vouches for these exact bytes. This trust can
  never authorize an official runtime id such as `codex`, `claude`, or `hermes`.
- `--catalog <file>` authorizes an official package through the signed catalog,
  verified against the trust anchor file shipped beside the daemon (see the
  [runtime catalog](../concepts/runtime-catalog.md#trust-anchor-file)). A host
  without an anchor refuses it (`official_trust_unavailable`); a host whose
  anchor exists but cannot be trusted refuses it with
  `official_trust_anchor_invalid`, and `pohunek doctor` reports the
  `catalog_trust_anchor` check as `warn` or `fail` respectively. Only a package
  whose catalog entry binds its package id, runtime id and digest may serve
  `codex`, `claude` or `hermes`; a catalog that authorizes a different id,
  package or digest for the archive is refused (`package_untrusted`). The
  daemon looks the archive's digest up in the verified catalog before it
  decompresses anything, so bytes the catalog does not list, a corrupted copy
  of a listed archive included, are refused with `package_untrusted` without
  being decoded. Installs trusted by `--sha256` are unaffected.
- `link <dir>` copies a developer directory. The copy is installed disabled and
  unselected, and is recorded with the `link` origin.

Packages are trusted local software, not a sandbox: the digest and the catalog
signature establish integrity and provenance, and an installed package can name
any program your account could run. [Workflow plugins](workflow-plugins.md)
are a separate class with their own CLI-owned store and `pohunek workflow`
install path; `pohunek plugin install` refuses a workflow archive because it
has no `runtime.toml`. Daemon, overlay, session worker and GUI extensions are
not packages at all; see "Runtime packages and trust" in `docs/architecture.md`.

## Consent

`install`, `link`, `update`, and `uninstall` change what the host may launch, so
they need `--yes`.

- Without `--yes`, the CLI asks the daemon to validate the package in a dry run
  (nothing is installed), prints what the package declares (package id,
  version, digest, trust origin, runtime id, display name, the program and fixed
  arguments the daemon will launch as the owner, the arguments it appends at
  launch to pass the session reference, the resume and fork argument templates
  (`{reference}` marks the reference slot), whether the first prompt is
  appended to the arguments, the version probe and the integration handler; the
  hook schema is part of the same descriptor), and fails with code `consent_required`. Nothing changed.
- With `--yes`, the CLI repeats the dry run and then performs the change.
- Under `--json` without `--yes` only the error envelope is printed; its
  message names the package, so omit `--json` to see the full review.

## Declaring a version probe

A descriptor names the supported release range with `version_probe` in
`[runtime]` (full reference in `docs/public-api.md`). Pick the parser by what
`<program> <args>` prints on its first line:

- `semver-v1` when it prints exactly `MAJOR.MINOR.PATCH` (Pi).
- `semver-line-v1` with a `line` template when the release sits inside fixed
  text: `line = "codex-cli {version}"` for `codex-cli 0.160.0`,
  `line = "{version} (Claude Code)"` for `2.1.289 (Claude Code)`,
  `line = "Hermes Agent v{version} {annotation}"` for
  `Hermes Agent v0.20.0 (2026.8.3)`.

The template is literal text, not a regular expression, and an invalid or
over-broad one (no literal next to `{version}`, an unknown placeholder, a
repeated `{version}`) fails package validation. A pre-release banner such as
`codex-cli 0.161.0-rc.1` never matches, so the runtime reports unsupported.
Verify the real banner of the runtime before declaring the template.

A release printed as `MAJOR.MINOR.PATCH-R<n>[.<m>]` (`1.4.13-R2`) is a numeric
revision, not a pre-release. Declare `parser = "semver-revision-line-v1"` with
a `line` template and `requires = ["version-probe-revision-v1"]`: it orders
`(MAJOR, MINOR, PATCH, n, m)` with an absent part as 0 (`1.4.13` equals
`1.4.13-R0`), and `min` and `below` accept both forms. `semver-v1` and
`semver-line-v1` keep refusing any suffix.

## Declaring input readiness

The `[input]` table may declare `ready`, the signal that shows the agent reads
initial input. `session.new` writes its initial input only after that signal, or
after the host's startup grace ends for a silent agent
(`input.initial_startup_grace_ms` in `service.toml`).

- `ready = "bracketed_paste"`, the default when the key is absent: the agent
  enabled bracketed paste, or its screen shows an editable prompt or idle
  evidence from the detection manifest.
- `ready = "screen_idle"`: only the screen-sourced idle evidence of the
  detection manifest counts. Declare it when the agent enables bracketed paste
  before its input accepts a paste, or discards input queued during its start,
  so the enable signal or the grace would release the input too early.
- `ready = "manifest_idle"`: the manifest's current winning evidence must be
  idle, from a screen region or the terminal title (idle read from OSC
  progress never counts). This is as strict as `screen_idle` — full grace, and
  `session_input_not_ready` plus a rollback when the deadline passes without
  that evidence — but a title-reporting runtime such as Codex can satisfy it.
  After a visible blocker, cached idle evidence stays ineligible across
  clears and redraws. Only a fresh winning screen or title idle signal after
  the blocker disappears releases the prompt; receiving the same title value
  again counts as fresh, while ticks and OSC progress do not.
  Declare it when bracketed paste precedes input readiness, and provide
  precise visible blockers for startup dialogs. A recognized blocker outranks
  an idle title; this policy does not positively require an initialized
  composer and can accept a title before any blocker has appeared. The
  [Codex guide](codex-package.md) lists its captured dialog coverage and limits.

The official packages declare:

The `[input]` table may also declare `startup_grace_ms`, the shortest wait for
that readiness the runtime needs to start, in whole milliseconds from 1 to the
longest grace the host accepts. For a client that sends
`extended_input_ready_wait` (the current CLI and SDK), the wait then lasts up to
the longer of it and the host's `input.initial_startup_grace_ms` (in
`service.toml`, 5000 ms on a fresh install), so a host raises it for a slower
machine but cannot shorten it below what the runtime declares. The grace is an
upper bound, not a delay: the input is written as soon as readiness shows, and
the full wait passes only when it never does. A client without that option
keeps the host's grace (`screen_idle` and `manifest_idle`) or the 500 ms
silent-reader bound.

The official packages declare:

| Runtime | `ready` | `startup_grace_ms` | Evidence |
|---|---|---|---|
| Claude | `screen_idle` | absent (host grace) | `live_prompt_box`: Claude drops pastes sent before its prompt box renders |
| Pi | `screen_idle` | absent (host grace) | `idle_editor_rule`: Pi enables bracketed paste before it draws any screen |
| Hermes | `screen_idle` | 30000 | `idle_prompt` or `tui_idle_status`: Hermes discards queued terminal input during its start, and its first start after install compiles its Python modules first |
| Codex | `manifest_idle` | absent (host grace) | `osc_title_idle`: Codex enables bracketed paste before its prompt reads input, and visible startup dialogs outrank stale titles (#1114, #1267) |

A `screen_idle` or `manifest_idle` session whose screen and title show no
readiness by the end of that wait fails `session.new` with
`session_input_not_ready`, nothing is written, and the session is rolled back.
A strict-readiness runtime gets the host's whole grace even from a client
without `extended_input_ready_wait`. The `[input]` table is parsed strictly: a
daemon of release 0.34.x rejects a descriptor that carries `ready` or
`startup_grace_ms`, so a package with either key needs a daemon that knows it.

A runtime definition declaring `screen_idle` requires a screen idle rule
(for example `live_prompt_box`); `manifest_idle` requires an idle rule on the
screen or terminal title (for example Codex's `osc_title_idle`). Profile
overrides follow different policies: a host profile over a
`screen_idle` base whose override manifest has no such rule, such as
`rules = []` or only working and blocked rules, loads, and its sessions use
`bracketed_paste` readiness; `pohunek doctor` reports that profile as a
warning (`agent_profile_<name>`), and adding an idle rule on a screen region
to the override restores screen readiness. A `manifest_idle` base never
downgrades this way: an override manifest without an eligible idle rule keeps
the strict readiness, so the input is refused instead of released by a signal
the runtime did not declare.

## Declaring program status

A detection manifest opts in to OSC 7501 program-status evidence with a table:

```toml
[program_status]
app = "my-agent"      # 1-32 characters of [A-Za-z0-9_.+-]
answer_probe = true   # optional, default false
```

`app` names the program whose root records the detector holds; records of any
other app, records without an app and records of children (an `id=` pair) are
ignored. `answer_probe = true` makes the session worker answer the program's
`ESC ] 7501 ; ? ESC \` probe, which programs such as Claude Code and Pi wait
for before they report; leave it off for a program that reports without a probe
(Codex). A rule may set `overrides_program_status = true`: when it is the screen
winner it beats a non-blocked record (for a state the program reports
misleadingly). The table and the flag are rejected when malformed, and a rule
flag without the table is an error.

The record is arbitrated with the screen rules (a visible dialog always wins
over a non-blocked record; a `blocked` record always wins; `working` reads
working; `done`, `idle` and `error` read idle), reported as `state_source =
osc_progress`, never used for input readiness, and dropped on `clear`, on a
detector reconfigure and after a lag resync. See "Agent state detection" in
`docs/architecture.md`. A host profile can switch the feature off for a
program that has an own switch (`PI_PROGRAM_STATUS=0`,
`CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1`); detection is then screen-only. The
official Pi, Codex and Claude packages declare the table; the other official packages do not yet.

## Versioning official packages

The `version` of an official package's `runtime.toml` names its archive. Two
different archives under one `package_id@version` cannot be told apart by
version, so any change to the archive of an official package (its descriptor,
manifests, hooks, or any other file in `runtime-packages/<p>/`) needs a
version above every version the last published release shipped for that
runtime. The pull request job `package version bump` enforces the rule with
`cargo xtask package bump`: it fails when the archive differs from every
released archive and the version is not strictly greater than the released
versions by semver precedence, so `1.0.0` after a released `1.0.0` fails, and
so do `0.9.9` and `1.0.0+build.1` (build metadata does not count).

- Bump the patch number for a fix, the minor number for an additive change such
  as a newly declared section, and the major number for a change that breaks
  installed sessions or hosts. Any bump above the released version satisfies
  the check.
- A package that is new to the release has no released version to exceed and
  passes. A package whose archive equals a released one needs no bump.
- The gate compares with the last published release at or below the workspace
  version, so it keeps working while the next version is being prepared. When
  the release catalog cannot be fetched, the job warns and the gate is
  undecided; a catalog that is present but does not verify fails it. A signed
  catalog carries an expiry, so once the last release's catalog has expired
  the gate fails every pull request until a newer release is published.

## Selecting a package version

`<package>` is a package id. Several versions of one id can be installed side by
side; narrow with `--digest <full digest or hex prefix of at least 12
characters>` and `--version <v>`.

- `inspect` with no narrowing shows the selected version, or errors listing the
  candidates when none is selected.
- `select`, `enable`, `disable`, and `uninstall` never guess: an ambiguous
  selection is the error `plugin_selector_ambiguous`, listing the candidates.
- A new version installed with `install` becomes selected only when it is the
  first version of its id. `update` installs enabled and selected and keeps the
  old version installed; `select` rolls back or forward. Profiles keep the
  digest they pinned, so an update never changes a pinned profile.
- `update` requires the id to be installed and the archive to declare the same
  package id.

### Incompatible updates stay installed but unselected

Sessions launched from an older version keep reporting through that version's
integration handler and hook schema until they end, so a version whose
integration differs cannot become the selected one while something still
references the old version. The integration of a version is the pair of its
`[integration]` handler id and hook schema id, or none; versions of one package
are compatible only when the pairs are equal (adding or dropping an integration
is a difference too), and a retained version whose descriptor or root cannot be
read counts as incompatible.

- A version is *held back* while a live, lost or resumable session or a host
  profile pin (the references that block `uninstall`) uses another version of
  the same package with a different integration. `plugin select` then fails with
  `package_integration_incompatible` and changes nothing. `plugin install` and
  `plugin update` still install the archive (enabled), report it as not
  selected, and name the digest it waits on; `update` exits with the select
  error because it promises a selected version. `plugin list` shows
  `selection blocked`, `plugin inspect` shows the `Selection:` line and the
  hook schema, and `plugin doctor` reports a `selection blocked by <digest>`
  finding.
- The selected version keeps serving fresh launches and nothing falls back to
  another package. A pinned profile or session still resolves from exactly its
  digest.
- The state is derived at each call from the registry and the sessions, never
  stored, so a daemon restart recomputes it. It clears when the last reference
  to the old version is gone (stop and remove the sessions, unpin or rebind the
  profiles, or uninstall the old version); the package is then selectable, but
  nothing selects it for you: run `plugin select` (or `plugin update` again).
  While a `metadata.jsonl` line that may pin a package cannot be read, every
  installed version counts as referenced, so a version stays held back with no
  session or profile naming it and `uninstall` fails with
  `package_retention_uncertain`. Stop the daemon, move the line named in the
  daemon log out of `metadata.jsonl` into a backup file and start the daemon.
- `package.bind_profile` (the wire method behind `plugin profile migrate`)
  refuses to pin a version whose integration differs from the selected version
  of its package or from another referenced version (the pin being replaced does not count), with the same error, because the pin would be a retained
  reference next to an incompatible selected version.
- A launch that resolved the previously selected version just before a select
  committed is refused with `runtime_package_changed`; retry it.

## Official aliases

An official package serves `codex`, `claude` or `hermes` as soon as it is
installed, enabled and selected; `plugin disable` or `plugin uninstall` leaves
the alias unserved. `plugin list` shows the serving package: the selected,
enabled `official` package whose runtime is the alias; without one nothing
serves it.

`codex`, `claude` and `hermes` have no built-in. Without its package, `session
new --agent codex` (or `claude`, `hermes`) is `agent_profile_not_found`, a host
profile with `base = "codex"` (or `"claude"`, `"hermes"`) and no
`package`/`digest` keys is `runtime_not_installed`, and `integration install
--agent codex` (or `claude`) is `runtime_not_installed`; each recovery hint names
`pohunek plugin install runtime/packages/<runtime>.tar.zst --catalog
runtime/runtime-catalog.json` (the profile hint then names `plugin profile
migrate <name>`). When the official package of that runtime (package id
`pohunek.runtime.<runtime>`) is installed but cannot be loaded,
the unpinned profile's launch is `runtime_incompatible` instead, and its
recovery names `pohunek plugin doctor` and then the migration, never the
install: a reinstall of the same archive is refused. `integration status`, `doctor` and `uninstall --agent codex`
(or `claude`) still reach the config home of the earlier built-in, so hooks
pohunek installed there can be reported and removed without the package. A
foreground `codex`, `claude` or `hermes` in a shell session and one started
outside pohunek are recognized only through the package's detection manifest
(and the Codex and Claude `[transcripts]` table), so without the package they
are not observed as that agent. A session launched from the built-in Codex,
Claude or Hermes of an earlier release is refused with
`runtime_builtin_removed` on resume, fork and every interaction (input,
attach, resize, rename, metadata) while nothing serves its runtime id; once the
package is installed, the official package adopts the session, live or stopped,
when the session's launch pin names the package id; the package version may
differ. The live session accepts input again, and its hook reports and processes
are attributed through the package. Resume and fork run through the package when
it honors the session's recovery contract: the same native reference kind, a
hook-reported (not core-assigned) reference, and for fork, fork support in both
the frozen spec and the package. The relaunched run is pinned to the package
digest. A Hermes fork answers `agent_fork_unsupported`, because Hermes declares
no native fork. A session whose contract the package cannot honor, and a
session record that names no package (v0.33.0), answer `runtime_served_by_package`;
the migration is a new session: `pohunek session new --agent <alias>`, which the
package serves. No command rebinds a session to another runtime source. Stop and
remove still work. The upgrade's runtime step installs the release's packages
and pins the profiles to them in the same run, so the manual `plugin install`
and `plugin profile migrate` steps are the fallback (see the
[update runbook](../runbooks/update-after-release.md#official-runtime-packages-during-an-upgrade)).
`pohunekd upgrade-preflight` reports an adoptable built-in
session as `adoptable`. A session that no package adopts is reported as
`would_lose_recovery` with code `runtime_source_not_served`, so `pohunek service
upgrade` refuses until `--accept-runtime-loss` is given. A session whose worker
runs a release before 0.34.0 journals no hook schema; if that worker journaled
an active identity, the identity cannot be verified, so the session is not
reattached: the preflight reports `would_not_be_adopted` with
`worker_identity_unverified`, and startup reports the runtime as a `conflict`
(`active_identity_provider_invalid`). Stop such a session with the running
release before the upgrade, or accept losing its runtime. A host profile on
that base can be pinned with `plugin profile migrate` once the package serves
the base (`package_profile_target_invalid` before). A migrated schema-1 binding
stays unpinned, so it is refused the same way.

## Disable

`disable` blocks fresh launches of the package, including launches through a
profile that pins the digest and relay-approved profiles. It does not stop live
sessions, and an existing session pinned to the package can still be resumed.
`enable` allows fresh launches again.

## Uninstall

The daemon refuses to uninstall a digest a live, lost, or resumable session or a
host profile pins (`package_referenced`). A package whose root fails
verification is removed only with `--remove-modified`; a root that verifies is
refused on that path (`package_root_intact`), so uninstall it without the flag.

## Doctor

`doctor [<package>]` verifies every installed package root and reports faults,
package roots on disk without a registry record, and pins of digests that are
not installed. It exits with status 1 when any finding exists and 0 otherwise.

## Profile migration

A host agent profile (`<config dir>/agents/<name>.toml`) keeps `base =
"<runtime id>"`. When an installed package serves that runtime, the profile
must also carry an explicit pin: `package = "<package id>"` and `digest =
"sha256:<64 hex>"`. A profile whose base is served by no installed package
carries neither. A profile moves to another digest only through
`pohunek plugin profile migrate`, never through `plugin update` or `plugin
select`.

`pohunek plugin profile list [--json]` shows every `agents/*.toml` with its
base, pinned package, and digest (abbreviated in the table, whole in JSON) and
one state:

- `builtin`: the base is `shell`, the one runtime built into the daemon;
  nothing to do.
- `pinned`: the pinned digest is installed and serves the base.
- `needs_migration`: an installed package serves the base but the pin is
  missing, incomplete, or names a package that does not serve the base.
- `package_not_installed`: the profile pins nothing, its base is not built in,
  and no installed package serves it, so its launches fail with
  `runtime_not_installed`. The detail names the recovery: for `codex`,
  `claude` or `hermes` the install of the official package from the release
  archive, for any other base a package that serves it, then
  `pohunek plugin profile migrate <name>`.
- `package_unavailable`: either the pinned digest is installed but its root or
  descriptor fails verification, so the daemon reports no runtime for it; or
  the profile pins nothing, no installed package serves its base, and an
  installed package that may serve it cannot be loaded. For `codex`, `claude`
  or `hermes` only an official package counts, because only one can serve an
  official alias. The repair depends on the fault (a missing root is restored
  by reinstalling the same archive, a modified one refuses that reinstall), so
  the detail names `pohunek plugin doctor`. A pinned profile needs only the
  repair, because it already names the digest; an unpinned one is then
  migrated.
- `pin_not_installed`: the pinned digest is not installed.
- `unreadable`: the file is a symlink or special file, is not owned by you, is
  group- or world-writable, is not valid TOML, or has an invalid `base`,
  `package`, or `digest`.

`pohunek plugin profile migrate <name> [--digest <d>] [--yes] [--json]` pins
the profile through the daemon's `package.bind_profile` method; the command
never writes the file. The daemon applies the pin under the same exclusive
package lifecycle authority as install, select and uninstall, so a concurrent
uninstall of the target either runs first (the bind then fails with
`package_profile_target_invalid`) or is refused with `package_referenced`.
The target is the installed package with the given digest (the full
`sha256:<64 hex>` or a unique prefix of at least 12 hex characters), which must
serve the profile's base and load without a fault. Without `--digest` it is the
one selected, enabled package serving the base; none or several is an error,
never a guess. The command first asks the daemon for a dry run, prints the
profile, base, package version, digest, and current pin, and changes nothing
until repeated with `--yes` (`consent_required` otherwise); the consented call
names the digest the preview showed. A base served by a built-in (`shell`) is
refused (`package_profile_base_builtin`), as are a missing profile
(`package_profile_not_found`), one the daemon cannot accept as a profile file
(`package_profile_unusable`: symlink, hard link, wrong owner, group- or
world-writable, too large, not valid TOML) and one edited while the bind ran
(`package_profile_changed`). A profile that already pins the target is reported
as `unchanged` with exit status 0 and is not written.

Only the `package` and `digest` keys are rewritten: existing ones are updated in
place, missing ones are inserted directly after `base`, and every other byte of
the file (comments, `[env]` values, ordering) is kept. The daemon reads the
profile with the same loader that serves launches and the retention scan, writes
the new text to a temporary file in the agents directory (mode narrowed to at
most `0600`), and renames it over `<name>.toml`: the name resolves to either the
old or the new complete file at every instant, with no interval in which the
profile is missing. Immediately before the rename the file is compared with what
was read and a changed profile is left as found. A temporary left by an
interrupted run never ends in `.toml`, is never loaded, and is removed by the
next bind. Profiles can hold secret `[env]` values, so nothing prints file
content or values; a parse failure is reported as a line number and message
only.

The commands work on this machine only (`--host` is rejected) and need the
agents directory to be owned by you and not group- or world-writable. A
migration changes the profile's revision on the daemon side, so a relay approval
of the previous revision goes stale.

## Shell completion

Dynamic completion (`pohunek completions <shell> --dynamic`) completes package
ids and `--digest` values from the local daemon within a short deadline, and
`plugin profile migrate` completes profile names from the agents directory
(names only, no daemon). Completion never starts the daemon and stays silent
when the daemon is unreachable. Archive and catalog arguments complete as files,
`link` as a directory.
