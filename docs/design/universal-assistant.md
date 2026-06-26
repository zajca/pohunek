# Design: Universal Pohunek Assistant

Status: **proposed** (RFC). This document defines the full target feature: one
universal assistant launched by `pohunek` as an ordinary agent session, capable
of setup, project configuration, updates, troubleshooting, and general help.

This is a complete assistant target. The user-facing goal is a full assistant
experience from the first shipped version of this feature.

---

## Objective

Add a first-class `pohunek assistant` command that starts one capable universal
assistant in a normal PTY-backed session.

The assistant is not a separate runtime and not a family of small task-specific
agents. It is one ordinary Codex, Claude, or host-profile agent session, started
with:

```text
one universal assistant
  + one on-disk knowledge bundle the agent reads directly
  + one small navigational opening prompt (not the knowledge itself)
  + one safety model
  + one redacted live environment snapshot (a file the agent reads)
  + one task intent that filters the navigation and steers the first response
```

The central mechanism is **knowledge-as-files, pulled on demand**, not a large
knowledge blob pushed into a single opening prompt. The selected agent is already
a coding agent with native file-reading tools; it reads the same Markdown the
humans read. The intent can be setup, project configuration, update,
troubleshooting, or general help. The agent implementation stays the same.

## User Experience Goal

The user should not need to run a long checklist before the assistant is useful.
The primary path is:

```bash
pohunek assistant
```

That command should do the right thing for a local host:

1. resolve paths;
2. start or verify the local daemon when needed;
3. choose a capable available agent, or explain the one missing requirement;
4. materialize the version-matched knowledge bundle for the session to read;
5. collect a redacted state snapshot and write it next to the bundle;
6. launch a normal assistant session with a small navigational prompt;
7. print the session id and attach command.

More specific commands only steer the same assistant:

```bash
pohunek assistant setup
pohunek assistant project --project ui
pohunek assistant update
pohunek assistant debug "launcher starts but no session appears"
pohunek assistant help "explain host agent profiles"
```

The command should feel like opening a knowledgeable coworker inside the current
`pohunek` environment, not like running a static wizard.

## Why One Universal Assistant

Setup, project configuration, update work, and help are not independent products.
They share the same concepts:

- daemon availability;
- host and remote targeting;
- project and worktree identity;
- launcher scripts and config;
- per-project actions/templates/prompts;
- host agent profiles;
- hooks;
- provider fetch and rendering boundaries;
- diagnostic JSON commands;
- source-code references.

Splitting these into separate assistants would duplicate knowledge and create
drift. The correct boundary is:

- **one assistant brain**: the shared knowledge bundle and safety rules;
- **many intents**: short navigation filter plus first-prompt steering for the
  current task.

## Non-Goals

- No separate LLM service or embedded model runtime.
- No hidden central coordinator.
- **No knowledge server (no MCP server, no daemon-side knowledge API).** The
  agent reads the knowledge bundle as ordinary files with the tools it already
  has. Knowledge delivery is pull-by-file, not push-by-prompt and not
  request-by-protocol.
- No separate daemon protocol just for assistant behavior, with one accepted
  addition: a host-side "materialize assistant bundle" capability for remote
  sessions (see Remote Materialization). v1 implements it; nothing else in the
  protocol changes.
- No fake sandbox. The assistant is a real agent session and can use the same
  tools the selected agent normally has.
- No daemon-side provider credentials. Linear/GitHub fetch remains caller-side.
- No claim that repo-local `.pohunek/` config is trusted just because the
  assistant wrote it.

## Command Surface

### Primary Command

```bash
pohunek assistant [OPTIONS] [REQUEST...]
```

Options:

```text
--intent <setup|project|update|debug|help>
--agent <name>
--host <host>
--project <id-or-label>
--repo <path>
--branch <branch>
--base-branch <branch>
--yes
--json
--print-prompt
--no-snapshot
--degraded
--no-start-daemon
```

The command launches an ordinary session. It does not create a special session
kind in the daemon.

With no `--intent` and no positional request, the default intent is `help`: the
prompt offers the root `index.md` without an aggressive filter, so the assistant
orients rather than assuming a setup task.

`--no-snapshot` skips live-state collection. It is a privacy/speed choice, not a
workaround for prompt size: the prompt no longer carries bulk knowledge, so it
cannot overflow from snapshot data.

`--degraded` is the only sanctioned way to launch without a readable knowledge
bundle (snapshot + source map only). It is explicit and never a silent fallback;
the default `pohunek assistant` fails rather than degrading (see Remote
Materialization and Error Handling).

### Convenience Commands

The CLI should expose intent wrappers:

```bash
pohunek assistant setup [REQUEST...]
pohunek assistant project --project <id-or-label> [REQUEST...]
pohunek assistant update [REQUEST...]
pohunek assistant debug [REQUEST...]
pohunek assistant help [REQUEST...]
```

These wrappers must call the same internal prompt builder and session launcher.
They only set `intent`.

### Print Prompt

`--print-prompt` prints the exact navigational prompt that would be sent to the
agent, including the resolved bundle path, the intent-filtered table of contents,
and the snapshot path, then exits without starting a session.

This is required for user trust, regression tests, and debugging the navigation.

## One-Command Bootstrap

The assistant command should handle local bootstrap directly.

Local behavior:

- If the daemon is running, use it.
- If the daemon is not running, start it in the background unless
  `--no-start-daemon` is set.
- If daemon start fails, return a clear error with the exact command to run
  manually.
- If setup assets are missing, still launch the assistant and include the missing
  setup state in the snapshot.

Remote behavior:

- Remote assistant launches preserve the existing `session new` safety model.
- A remote assistant needs a target project or repo, just like remote
  `session new`.
- `--yes` is still required for non-interactive remote starts.
- The knowledge bundle is materialized on the host that runs the agent, version-
  matched to that host's binary (see Remote Materialization).

This keeps the default path short without hiding remote execution behind a new
implicit trust decision.

## Agent Selection

The assistant must start with a capable coding agent, not a plain shell.

Resolution order:

1. `--agent <name>` wins.
2. A configured assistant default wins when present.
3. If no default exists, the CLI probes host capabilities and chooses an
   available capable runtime using a documented ranking.
4. If no capable runtime is available, the command fails with a recovery message.

The selection is not silent: human output and JSON output both report which
agent was selected and why.

Suggested ranking:

1. a host profile named `pohunek-assistant`;
2. `codex`;
3. `claude`;
4. other host profiles whose base kind is `codex` or `claude`.

The ranking is only a launch policy. Users can always override it with
`--agent`. The `pohunek-assistant` profile at the top of the ranking is
user-defined; `pohunek setup` may scaffold a commented template for it but never
auto-enables it.

The selected agent must have filesystem read tools, because knowledge delivery is
pull-by-file. Every runtime in the ranking does. There is **one agent-agnostic
opening prompt** (a single `system.md`); it points at paths and never assumes
specific tool names, so it works unchanged across Codex, Claude, and host
profiles. Per-agent tuning can be added later behind the same prompt contract
without splitting `system.md`.

### Read-Access Preflight

Having read tools is not enough: the agent may run in a sandbox, container,
different working directory, or otherwise restricted filesystem where the
materialized knowledge directory and snapshot file are not
reachable. Pull-by-file silently degrades to a useless assistant if the agent
cannot see its own knowledge.

Therefore the launch performs an explicit read-access preflight: it verifies that
the resolved agent profile's execution context can read the materialized
knowledge directory and the snapshot file before sending `session.new`. The check
must account for the agent's actual filesystem view (sandbox/container/cwd), not
just the launcher's view.

- If the check passes, launch normally.
- If the check fails, the command **fails before `session.new`** with the
  unreachable path, the agent's effective filesystem constraint, and a concrete
  remedy (materialize inside the agent's accessible root, relax the sandbox, or
  pick a profile with access). The assistant must never launch pointing at a
  knowledge path the agent cannot read.

For profiles whose filesystem view cannot be determined statically, prefer
materializing into a path known to be inside the agent's accessible root over
guessing, and record the chosen strategy in the snapshot.

## Knowledge and Documentation Pipeline

The assistant must not be the only consumer of pohunek knowledge. One source
corpus feeds two consumers from the **same files**:

- humans, who read it rendered (online site, offline bundle, or plain GitHub);
- the assistant agent, which reads it as raw Markdown on disk.

```text
docs/knowledge/        single source of truth (Markdown + frontmatter)
  |
  | pohunek docs build
  v
versioned bundle  ──►  rendered site / offline docs        (humans)
                  └─►  raw Markdown materialized at launch  (agent)
```

There is no separate "assistant knowledge" maintained apart from the docs. The
assistant reads the same concepts a human would, navigated the same way. The
assistant does not invent the source of truth, and there is no second copy to
drift against.

### Bundle Format: OKF as Inspiration, Local Schema as the Contract

The bundle borrows its shape from the Open Knowledge Format (OKF) draft
(`https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md`).
The useful ideas are:

- a knowledge bundle is a directory tree;
- each concept is one UTF-8 Markdown file with YAML frontmatter and a required
  `type`;
- `index.md` files provide progressive-disclosure navigation;
- `log.md` files record local bundle history;
- relative Markdown links connect related concepts;
- citations point back to code, generated reference, or design sources.

`pohunek` treats OKF as **inspiration only**, not as an external dependency and
not as a conformance target. OKF is deliberately permissive (it tells consumers
to tolerate unknown types, missing fields, and broken links); that permissiveness
serves cross-organization exchange, which a single-user tool does not need.
`pohunek` therefore validates the bundle against its **own strict local schema**
and does not claim OKF conformance in any artifact. The manifest records a local
`knowledge_schema_version`, not an OKF version.

Two practical consequences:

- Links must be **relative** so they resolve both on disk (for the agent) and
  when rendered on GitHub or the docs site (for humans). Do not use
  `pohunek://...` URIs in link targets; a person cannot click those. Stable
  addressability is provided by a separate `id` frontmatter field (see Local
  Knowledge Schema), not by link form, so concepts can move without breaking
  manifests, redirects, or the search index.
- Unknown frontmatter fields are tolerated when reading, but the prompt/
  navigation composer uses a **field allowlist** (see Redaction), so no arbitrary
  frontmatter ever reaches the agent prompt by default. The allowlist protects
  only the prompt; it does not protect the bundle bodies, which the agent reads
  directly from disk. Bundle bodies are governed by the public-safe rule below.

### Local Knowledge Schema

Every non-reserved knowledge file under `docs/knowledge/` is a concept document
with frontmatter:

```yaml
---
type: Guide
id: guide/setup
title: Local setup
description: Configure the local daemon, launcher assets, and agent hooks.
tags: [setup, local, assistant]
intents: [setup, help]
source_kind: manual
since: 0.4.0
---
```

Required fields:

- `type`: one of the local concept types listed below.
- `id`: stable concept identifier (e.g. `guide/setup`), independent of file
  path. It anchors addressability, manifests, redirects, and the search index, so
  a concept can be moved or renamed on disk without breaking references. Must be
  unique within the bundle.
- `title`: human-readable page title.
- `description`: one sentence for index generation, search snippets, and the
  intent-filtered table of contents.
- `source_kind`: `manual`, `generated`, or `snapshot-template`.

For **behavior-bearing types** (`CliCommand`, `ConfigReference`,
`ProtocolMethod`, `ProtocolEvent`, `Runbook`), `since` is also **required**: the
assistant must never give version-inaccurate advice about behavior, so every such
concept must state the first version it describes.

Recommended fields:

- `tags`: cross-cutting filters.
- `intents`: assistant intents this concept is useful for (`setup`, `project`,
  `update`, `debug`, `help`). Drives the per-launch table of contents.
- `generated_from`: source file or command for generated concepts.
- `since`: first `pohunek` version whose behavior this concept describes.
- `changed_in`: list of versions where the described behavior materially changed.
- `deprecated`: version where the behavior was removed or replaced, plus a link
  to its successor concept.
- `citations`: short list of source files, generated references, or external
  URLs supporting important claims.

`since` / `changed_in` / `deprecated` exist because the assistant must never give
version-inaccurate advice. The materialized bundle matches the binary that
launched it, but the source tree the agent can also read (via the source map) may
be a different version; these fields let the agent reason about that skew.

Local concept types:

```text
Concept
Guide
Runbook
Troubleshooting
SafetyPolicy
CliCommand
ConfigReference
ProtocolMethod
ProtocolEvent
SetupAsset
PromptTemplate
SourceMap
SnapshotTemplate
ReleaseNote
```

CI rejects missing required fields, invalid local `type` values, malformed
frontmatter, broken internal links, duplicate or missing `id`, and a missing
`since` on any behavior-bearing concept. This is a deliberately stricter contract
than OKF; it is the guardrail that lets one corpus serve both humans and the
agent.

### Sources of Truth

Use four source categories.

1. **Manual conceptual documentation.**
   Human-authored explanations: architecture, mental model, workflows, setup
   guides, troubleshooting, security/trust model, examples, and release/update
   notes. These carry `source_kind: manual`.

2. **Generated reference from code.**
   Produced from the implementation at every build (never committed), so it
   cannot drift or go stale. These carry `source_kind: generated` and
   `generated_from`:

   - CLI command reference from the `clap` command tree and help output;
   - protocol method and event reference from `crates/protocol`;
   - JSON payload shape reference from protocol structs;
   - config reference for `launcher.conf`, `templates.toml`, `actions.toml`,
     `agents/*.toml`, hook names, and prompt variables;
   - setup asset reference from embedded scripts and default templates.

3. **Tested runbooks.**
   Executable operational guides for common tasks:

   - first setup;
   - launcher setup;
   - project setup;
   - remote host setup;
   - agent profile setup;
   - hook setup and review;
   - update after release/source changes;
   - troubleshooting.

4. **Runtime snapshot.**
   Live facts collected at assistant launch: doctor output, host capabilities,
   project state, session state, selected config file existence, and redacted
   local paths. This is not part of the committed bundle; it is written as a file
   next to the materialized bundle at launch time.

### Repository Layout

There is exactly one knowledge layout, and only **hand-authored** concepts are
committed. There is no second hand-maintained "pack" checked into the crate
source, and **generated reference is never committed** (see Build below): it is
produced at build time and injected into the bundle under `reference/`.

```text
docs/
  knowledge/          committed source: manual + snapshot-template concepts only
    index.md
    log.md
    concepts/
      architecture.md
      sessions.md
      projects.md
      worktrees.md
      agent-profiles.md
    guides/
      setup.md
      project-setup.md
      remote-hosts.md
      launcher.md
    runbooks/
      debug-daemon.md
      debug-launcher.md
      update-after-release.md
    safety/
      trust-model.md
      secrets.md
      repo-pohunek.md
    assistant/
      system.md
      source-map.md
    # reference/ is NOT here; it is generated at build (see below)
```

`docs/knowledge/` is the committed source bundle of hand-authored concepts
(`source_kind: manual` or `snapshot-template`). `docs/knowledge/assistant/` holds
the assistant's mission text (`system.md`) and the source map; it is not a
separate corpus.

Generated concepts (`source_kind: generated`) are produced from current code at
every build into `reference/cli/`, `reference/config/`, and `reference/protocol/`
and merged into the normalized bundle. They are never committed, so there is
nothing to drift against; humans see them in the rendered site and offline docs,
which are built from the same merged bundle. The build directory is ignored:

```text
target/pohunek-docs/
  knowledge-bundle/   normalized bundle (manual + generated), embedded into CLI
  site/               rendered human site
  offline/            offline human docs
  manifest.json
```

### Embedded Bundle and Materialization

The build runs before `cargo build`: a `build.rs` (or the `docs build` step)
generates the reference concepts into `OUT_DIR`, merges them with the committed
hand-authored concepts from `docs/knowledge/`, normalizes the result, and the
crate embeds that merged bundle (the same embed approach already used for setup
assets). Nothing generated is committed; the embed source is the merged bundle in
`OUT_DIR`. Consequences:

- the bundle always matches the binary version that launched the assistant;
- it works for release installs with no source checkout present;
- generated reference is rebuilt from current code on every compile, so it cannot
  go stale and there is nothing to drift against.

At launch, the binary **materializes** the embedded bundle. The bundle is
**version-shared**: it is extracted once per binary version into a shared cache
and reused across sessions; only the snapshot is per-session.

```text
$XDG_CACHE_HOME/pohunek/knowledge/<version-hash>/   shared bundle (extract once)
$XDG_RUNTIME_DIR/pohunek/assistant/<session-id>/
  snapshot.json                                     per-session redacted snapshot
```

The host that owns the session owns materialization: the CLI for local sessions,
the daemon for remote ones (via the materialize capability). If the shared
version directory already exists, extraction is skipped. Stale version
directories are garbage-collected when the binary version changes; the
per-session snapshot is removed on session end.

The opening prompt references the resolved knowledge directory and snapshot file.
The agent reads `index.md`, follows links, and reads `snapshot.json` itself.
Materialization is the only thing the launch does with the knowledge; it never
inlines bundle bodies into the prompt. Neither path may be inside the repo
working tree.

### Remote Materialization

For a remote session the agent runs on the remote host, so the bundle must exist
on the **remote** filesystem, version-matched to the **remote** binary (not the
local CLI's). The local CLI composes only prompt text (pointers), never ships its
own bundle bytes over the wire, so a version-mismatched bundle can never reach a
remote agent.

v1 adds exactly one daemon capability for this: "materialize the embedded
assistant bundle for session `<id>` and return its path." The remote daemon
extracts its own version-matched bundle into its own shared cache; the local CLI
never ships bundle bytes over the wire.

The fallback is **strict, not soft**. A remote assistant without a remote
materialized bundle that the agent can read is a degraded assistant, and this is
the full-featured target, so:

- If the remote host can materialize and the read-access preflight passes against
  the remote path, launch normally.
- Otherwise the command **fails before launch** with the reason (daemon lacks the
  capability, or the agent cannot read the materialized path) and the manual
  remedy. It does not start a knowledge-less or source-map-only session under the
  `assistant` name.
- A reduced session is only ever produced under an explicit, separately named
  opt-in (e.g. `--degraded`), never as a silent fallback of `pohunek assistant`.

### Build Command

The project should have one documentation build entry point:

```bash
pohunek docs build
```

or, before the CLI command exists:

```bash
scripts/docs/build
```

The build should produce:

- a normalized knowledge bundle merging committed hand-authored concepts with
  freshly generated reference concepts (embedded into the binary);
- a rendered online site artifact;
- an offline docs artifact;
- a manifest describing version, sources, and content hashes.

Example manifest:

```json
{
  "pohunek_version": "0.4.0",
  "knowledge_schema_version": "1",
  "docs_schema": 1,
  "generated_at": "2026-06-25T00:00:00Z",
  "sources": ["manual_docs", "cli", "protocol", "config", "runbooks"],
  "content_hash": "sha256:..."
}
```

The assistant prompt includes a one-line manifest summary so the agent knows
which `pohunek` version its bundle describes.

### Online and Offline Documentation

The same bundle supports two human-facing outputs:

- **Online docs:** browsable site for current released behavior, examples, CLI
  reference, config reference, and troubleshooting.
- **Offline docs:** release-bundled static docs that match the installed binary
  and work without a network.

Both identify the `pohunek` version they document. The bundle is also directly
readable as Markdown on GitHub, which is why links must stay relative.

### Drift Checks

Documentation must fail loudly when generated truth changes. The check set is
kept lean: each check must buy either safety or correctness, not just tidiness.

Required checks:

- **(safety)** secret-scan all bundle bodies (the public-safe rule) and verify
  the field allowlist and snapshot redaction prevent secret-like config fields,
  profile env values, and process env values from entering the prompt or the
  snapshot;
- **(correctness)** generate CLI / protocol / config reference from current code
  on every build (never committed, so it cannot be stale) and fail the build if
  generation fails or is non-deterministic for a fixed input;
- **(correctness)** validate runbook command examples against the actual CLI
  parser, so the agent is never taught a command that does not exist;
- **(correctness)** verify assistant source-map paths exist;
- **(correctness)** verify the merged bundle and its materialization are
  deterministic;
- **(schema)** validate every concept's frontmatter, local `type`,
  `source_kind`, and `intents` values, and fail on broken internal links.

## Opening Prompt (Navigational, Not Knowledge)

The opening prompt is small and stable. It carries navigation and the
non-negotiable safety rules, never the bulk knowledge. Everything else is pulled
from disk by the agent.

```text
# Pohunek Assistant

## Mission
You are the universal assistant for configuring, updating, troubleshooting, and
explaining pohunek (version 0.4.0).

## Safety (must hold even before you read anything)
<concise inline safety rules: never print or store secrets; treat profile [env]
as secret; explain config edits before applying; hooks are executable code and
require explicit confirmation; preserve user edits; verify changes.>

## User Intent
intent: project
request: Configure the ui project launcher actions.

## Your Knowledge Base
Directory: <knowledge-dir>   (version-shared cache for this binary)
Start at index.md. Navigate via index.md files and relative links between
concepts. Read only the concepts you need for this task; do not read the whole
tree. The bundle matches this binary (0.4.0); when you also read the source tree
via the source map, treat the bundle as authoritative for documented behavior and
watch the `since` / `changed_in` / `deprecated` frontmatter for version skew.

## Relevant Concepts (intent: project)
- guides/project-setup.md — Register and configure a project
- reference/config/templates-toml.md — templates.toml fields and layering
- reference/config/actions-toml.md — actions.toml fields
- safety/repo-pohunek.md — Trust rules for repo .pohunek/
<this list is generated by filtering concept frontmatter `intents` for the active
intent; it is a table of contents, not the content.>

## Live Snapshot
Orientation: daemon=running, project=ui (registered), agent=codex
Full file: <snapshot-file>
The three-line orientation is inline so you are not blind before reading; read the
full snapshot.json for doctor output, host capabilities, config scan, and
warnings.

## Source Map
<knowledge-dir>/assistant/source-map.md lists where to verify implementation
details against the actual source tree when precision matters.

## First Step
Read the snapshot, open the relevant concepts, identify the next concrete action,
and proceed using documented pohunek commands and file edits. Verify changes
before claiming they work.
```

The prompt-size failure mode is gone: the prompt is bounded by the intent-filtered
table of contents (tens of lines), not by the size of the knowledge or snapshot.

## Live Snapshot

The launch writes a compact redacted snapshot to the per-session `snapshot.json`
and also derives a **three-line orientation summary** (daemon state, selected
project, selected agent) that is inlined into the prompt, so the agent has
immediate orientation without a read while the full state stays in the file.

Recommended sections:

```text
assistant:
  intent
  user_request
  selected_host
  selected_project
  selected_agent
  auto_started_daemon
  knowledge_bundle_version

paths:
  config_dir
  data_dir
  log_dir
  launcher_bin_dir
  sway_config_dir
  knowledge_dir

doctor:
  overall
  checks

host:
  capabilities
  supported_agents
  runtimes

projects:
  selected_project
  known_projects_summary
  selected_project_actions

sessions:
  active_sessions_summary

config_scan:
  launcher_conf_status
  prompt_names
  templates_file_status
  actions_file_status
  agent_profile_names
  hook_names

source_tree:
  git_root
  git_branch
  dirty_status_summary
  version_matches_binary
```

Snapshot collection is best-effort. A failed item becomes a warning inside the
snapshot file, not a reason to lose all context. Because the snapshot is a file
the agent reads on demand, a partial snapshot does not silently truncate the
agent's view: the warnings are visible and the agent can re-run the underlying
`--json` command itself.

### Public-Safe Bundle Bodies

The allowlist governs only what enters the prompt. It does **not** govern the
knowledge bundle, because the agent reads bundle Markdown directly from disk and
a human may publish the same bundle as online/offline docs. The security rule for
the bundle is therefore stated at the body level, not the field level:

> The committed and materialized knowledge bundle is treated as **public-safe**.
> No bundle body — manual concept, generated reference, runbook, or snapshot
> template — may contain a secret value.

This is enforced two ways:

- generated concepts are produced only from non-secret sources (CLI help,
  protocol structs, config schemas, embedded script names — never env values or
  resolved secrets);
- a **secret scan** runs over the whole bundle in CI, failing the build if any
  body matches secret-like patterns (keys, tokens, `[env]` values, credentials).
  Because the materialized bundle is byte-identical to the embedded one, the CI
  scan also covers what the agent reads at launch. A leak via a hand-written docs
  file or a generated page is caught here, not at prompt time.

### Snapshot Redaction (Allowlist, Not Denylist)

The snapshot and the prompt are built from an **explicit allowlist** of fields.
Anything not on the allowlist is excluded by default; a new config field cannot
leak just because nobody remembered to add it to a blocklist.

Rules:

- The snapshot serializer accepts only known, listed fields; it must be unable to
  emit an unknown field. A test asserts this.
- The prompt/table-of-contents composer reads only an allowlisted set of
  frontmatter fields (`type`, `id`, `title`, `description`, `intents`, `since`,
  `changed_in`, `deprecated`); other frontmatter never reaches the prompt.
- Allowlisted content includes filenames, existence, parse status, selected
  action names, and existing `--json` command output.
- Process environment variables, profile `[env]` values, hook script bodies, and
  arbitrary config file bodies are never collected.
- Prompt template content is included only when directly relevant to the selected
  project/action, and only via the allowlist.

The agent can later read specific files when useful. The initial snapshot stays
conservative.

## Safety Model

The assistant is intentionally capable. It can inspect and edit files through the
underlying agent. The safety model is a working agreement plus existing
`pohunek` and filesystem controls plus one hard gate.

The assistant must:

- explain intended config edits before making them;
- preserve user edits unless explicitly asked to overwrite;
- avoid committing or storing secrets;
- treat agent profile env values as secret-bearing;
- avoid weakening owner-only, name-guard, and containment checks;
- preserve remote confirmation behavior;
- review repo `.pohunek/` hooks like executable code;
- prefer structured `--json` inspection commands;
- verify changes after applying them.

The assistant may:

- write host config when that is the requested task;
- write repo `.pohunek/` config when configuring that project;
- create or modify prompts, actions, templates, and profiles;
- run setup commands;
- start and inspect sessions;
- read relevant source and docs.

### Hard Gate: Hook Writes

Hooks execute code on subsequent sessions, so an assistant-written hook is an
elevated-privilege action, not an ordinary config edit. Therefore:

- Creating or modifying any hook (host `hooks/*` or repo `.pohunek/hooks/*`)
  **always requires explicit, per-file interactive confirmation**, independent of
  `--yes`. `--yes` covers ordinary non-interactive starts; it does **not** cover
  hook writes.
- In a non-interactive context with no way to confirm, the assistant must not
  write the hook. It writes the proposed hook to a clearly named quarantine path
  and tells the user the exact command to review and promote it.

This is a hard control, not a model-discretion request, because the soft "explain
before editing" rule alone is insufficient for code that runs later.

## Intent Behavior

Intent selects the navigation filter (the table of contents) and steers the first
response. It does not split the implementation. With no intent given, the default
is `help` (root index, no aggressive filter).

### Setup

The assistant checks daemon health, setup assets, launcher config, sway drop-in,
agent runtimes, integration hooks, NetBird status, and basic project readiness.

It should propose and apply a coherent setup path, not hand the user a long
manual checklist.

### Project

The assistant targets a specific project or infers the local current project when
safe. It checks project registration, `project show`, resolvable actions,
template/prompt layers, base branch behavior, and worktree implications.

It can create or edit `.pohunek/templates.toml`, `.pohunek/actions.toml`,
`.pohunek/prompts/*.tmpl`, and project hooks (under the hook hard gate) when the
user wants project-level configuration.

### Update

The assistant helps reconcile installed setup assets, host config, project
definitions, and docs after a source or binary update. The bundle's `changed_in`
and `deprecated` frontmatter is the primary signal for what changed between
versions.

It must avoid clobbering edited files. It should show diffs or explain file
changes before applying them.

### Debug

The assistant follows a debugging loop:

1. understand the failure;
2. collect structured state (read the snapshot, run `--json` commands);
3. inspect relevant files;
4. form a concrete hypothesis;
5. test it;
6. apply the smallest fix;
7. verify.

### Help

The assistant answers from the knowledge bundle, navigating from `index.md`, then
uses the source map when exact behavior matters.

## Data Flow

```text
user
  |
  | pohunek assistant <intent/request>
  v
CLI assistant command
  |
  | bootstrap daemon if local and needed
  | select agent + read-access preflight
  | materialize bundle (version-shared cache; daemon does it for remote)
  | collect redacted snapshot   -> per-session snapshot.json
  | compose small navigational prompt (TOC filtered by intent + 3-line summary)
  v
session.new
  |
  | ordinary daemon request with initial input (the navigational prompt)
  v
daemon-owned PTY session
  |
  v
selected agent profile
  |
  | reads <knowledge-dir>/** and the snapshot file on demand with its own tools
  v
work
```

No special daemon-side assistant runtime is required. For local sessions the
feature reuses the existing session lifecycle and control protocol unchanged; the
only possible protocol addition is remote bundle materialization.

## Output

Human output:

```text
started assistant session: local/s-123
agent: codex
intent: setup
knowledge: 0.4.0 (materialized)
snapshot: included
attach: pohunek attach local/s-123
```

JSON output:

```json
{
  "session": { "...": "SessionInfo" },
  "assistant": {
    "intent": "setup",
    "agent": "codex",
    "knowledge_bundle_version": "0.4.0",
    "snapshot_included": true,
    "auto_started_daemon": true
  }
}
```

## Error Handling

- Daemon start fails: report the exact failure and manual recovery command.
- No capable agent runtime: report available runtimes and suggest installing or
  configuring one.
- Remote target missing project/repo: fail before dialing, matching existing
  remote session behavior.
- Bundle materialization fails: fail before `session.new` with the target path
  and reason; the assistant must not launch pointing at a missing knowledge dir.
- Agent cannot read the materialized path (read-access preflight fails): fail
  before `session.new` with the unreachable path, the agent's filesystem
  constraint, and a concrete remedy.
- Remote materialization gap (daemon cannot materialize, or remote preflight
  fails): fail before launch with the reason and manual remedy. Do not start a
  knowledge-less session under the `assistant` name; a reduced session requires
  the explicit `--degraded` opt-in.
- Snapshot item fails: include a warning in `snapshot.json` and continue.
- Initial input not confirmed: surface the existing warning and do not claim the
  assistant received its opening prompt.

## Testing Strategy

### Unit Tests

- intent parsing maps every command form to the same internal launch model;
- agent selection is deterministic and visible;
- the navigational prompt is stable and never embeds bundle bodies;
- the intent-filtered table of contents is derived deterministically from concept
  frontmatter;
- `--print-prompt` never starts a session;
- the snapshot serializer cannot emit a non-allowlisted field;
- the prompt composer reads only allowlisted frontmatter fields;
- daemon bootstrap policy is covered;
- embedded-bundle materialization is deterministic for a fixed manifest.

### CLI Tests

- `pohunek assistant --print-prompt setup` prints the navigational prompt, the
  resolved bundle path, and the filtered TOC, then exits;
- `pohunek assistant setup --json` starts or uses the daemon, materializes the
  bundle, writes the snapshot, and sends `session.new` with the composed `input`;
- remote assistant launch preserves `--yes` behavior;
- the read-access preflight fails the launch before `session.new` when the agent
  context cannot reach the materialized path (e.g. a sandboxed profile);
- a remote launch that cannot materialize a readable bundle fails before launch
  unless `--degraded` is set;
- hook writes require confirmation even with `--yes` (the hard gate);
- project intent includes the project-filtered TOC and project context;
- human output includes the attach command and the knowledge version;
- JSON output includes assistant metadata.

### Snapshot Tests

- doctor report is embedded when available;
- host capabilities are embedded when available;
- selected project actions are embedded when available;
- config parse errors become snapshot warnings;
- profile env keys and values never appear in the snapshot or prompt.

### Documentation Tests

- source-map paths exist;
- every `docs/knowledge/` concept has valid frontmatter with required local-
  schema fields, a unique `id`, and (for behavior-bearing types) a `since`;
- reserved `index.md` and `log.md` files follow the local bundle rules;
- internal bundle links resolve and are relative;
- the secret scan over all bundle bodies passes (public-safe rule);
- runbook commands match the CLI grammar;
- examples stay synchronized with parser tests;
- generated CLI, protocol, config, and setup-asset references generate cleanly
  from current code and are deterministic for a fixed input;
- the embedded bundle, online docs, and offline docs are built from the same
  manifest;
- the offline docs manifest version matches the binary version used in tests;
- bundle, prompt, and snapshot never include profile env values, process env
  values, or other secret-like fields.

### Behavior Eval

Composition tests prove the plumbing; they do not prove the assistant is useful
or correct. Add a small golden-task eval:

- a fixture set of seeded environment states (e.g. "daemon down", "launcher
  misconfigured", "project not registered", "stale setup assets after update");
- for each, an expected concrete outcome (the action the assistant should take or
  recommend) and a hard failure on hallucinated commands;
- the eval runs the real assistant against the materialized bundle and asserts the
  outcome, so regressions in knowledge content or navigation are caught, not just
  regressions in prompt assembly.

Because it drives a real agent (token cost, non-determinism), the behavior eval
runs as a **local/manual release gate** over one runtime (default `codex`), not as
a blocking per-PR CI job. The deterministic checks — schema validation, drift,
secret scan, prompt composition — stay in CI and gate every PR.

## Definition of Done

The feature is complete only when the full assistant experience works:

- `pohunek assistant` launches a useful local assistant in one command.
- The command can bootstrap the local daemon when needed.
- Setup, project, update, debug, and help intents all work through the same
  assistant implementation, differing only by navigation filter and steering.
- Agent selection is explicit in output and overrideable with `--agent`.
- Knowledge is delivered by file: the embedded bundle (committed hand-authored
  concepts plus build-time generated reference, nothing generated committed) is
  materialized version-matched to the binary into a version-shared cache, and the
  opening prompt is a small navigational prompt (mission, safety, intent, bundle
  path, intent-filtered TOC, three-line snapshot summary, snapshot path, source
  map) that never embeds bundle bodies.
- `--print-prompt` exposes the exact navigational prompt without launching.
- Project intent includes project registration and action context.
- A read-access preflight confirms the agent's filesystem context can read the
  materialized knowledge dir and snapshot; launch fails before `session.new`
  otherwise.
- Remote launches preserve existing safety gates; remote materialization is
  implemented, and a host that cannot materialize a readable bundle fails before
  launch (never a silent or knowledge-less `assistant` session). A reduced
  session requires the explicit `--degraded` opt-in.
- The knowledge bundle is public-safe: a CI secret scan over all bundle bodies
  passes, and snapshot and prompt are allowlist-built so profile env and process
  env values cannot enter them.
- Hook writes are gated by explicit per-file confirmation independent of `--yes`.
- `docs/knowledge/` is a valid local-schema bundle with checked frontmatter,
  reserved files, relative internal links, and concept types.
- The embedded bundle, online docs artifact, and offline docs artifact are built
  from one documentation pipeline and share a manifest.
- Generated CLI, protocol, config, and setup-asset reference is produced from
  current code on every build (never committed), and runbook examples are
  validated against the CLI parser.
- Tests cover prompt composition, navigation/TOC derivation, bootstrap, agent
  selection, materialization, read-access preflight, snapshot allowlist, bundle
  secret scan, local launch, remote launch (and remote fail-before-launch), the
  hook gate, project intent, docs generation, schema validation, docs drift, and
  the behavior eval.

## Delivery Scope

This should land as one coherent feature, not as a narrow setup-only assistant
followed by a series of product expansions. Engineering can still split the work
internally across parser, prompt composition, snapshot collection, bundle
embedding/materialization, bootstrap, launch wiring, docs generation, and tests,
but the shipped user-facing capability is the complete universal assistant
described here.

The stable internal contract is the **bundle**: its layout, schema, and
materialized path. Manual concepts can be authored first and generated concepts
filled in behind the same contract without changing the assistant, the prompt, or
the launch path.

## Summary

The assistant is one full-featured, universal agent. Knowledge lives in one
documentation corpus that humans read rendered and the agent reads as files;
intent only filters navigation and steers the opening prompt; it does not split
the system into separate assistants and it does not require a knowledge server.

"Knows everything" means:

- one documentation corpus shared by humans (rendered) and the agent (raw files);
- an OKF-inspired bundle validated by a strict local schema, stays readable by
  humans and traversable by the agent;
- the bundle embedded in the binary and materialized version-matched at launch,
  so the agent's knowledge always matches the tool that started it;
- a small navigational prompt plus pull-by-file delivery, so there is no context
  ceiling and no prompt-size cliff;
- redacted live context written as a file the agent reads on demand;
- source-map access to the repo for verifying exact behavior;
- exact CLI runbooks;
- a safety model that keeps config and secret boundaries clear, with a hard gate
  on hook writes.

That gives the user a powerful one-command assistant while keeping `pohunek`
aligned with its existing architecture.
