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
agents. It is one ordinary Codex, Claude, or host-profile agent session launched
with a rich opening prompt:

```text
one universal assistant
  + one curated product knowledge pack
  + one safety model
  + one redacted live environment snapshot
  + one task intent that steers the first response
```

The intent can be setup, project configuration, update, troubleshooting, or
general help. The agent implementation stays the same.

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
4. collect a redacted state snapshot;
5. launch a normal assistant session with the knowledge pack injected;
6. print the session id and attach command.

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

- **one assistant brain**: the shared knowledge pack and safety rules;
- **many intents**: short first-prompt steering for the current task.

## Non-Goals

- No separate LLM service or embedded model runtime.
- No hidden central coordinator.
- No separate daemon protocol just for assistant behavior unless an existing
  protocol gap forces one.
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
--no-start-daemon
```

The command launches an ordinary session. It does not create a special session
kind in the daemon.

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

`--print-prompt` prints the exact prompt that would be sent to the agent and
exits without starting a session.

This is required for user trust, regression tests, and debugging the knowledge
pack.

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
`--agent`.

## Knowledge Pack

The assistant should carry a curated knowledge pack built into the CLI release.
It is versioned with the binary so the assistant knows the semantics of the
`pohunek` version that launched it.

Recommended source layout:

```text
crates/cli/src/commands/assistant/
  mod.rs
  prompt.rs
  snapshot.rs
  assets/
    system.md
    knowledge.md
    safety.md
    runbooks.md
    source-map.md
```

Responsibilities:

- `prompt.rs`: pure prompt composition from intent, snapshot, request, and
  embedded assets.
- `snapshot.rs`: redacted local and daemon-backed context collection.
- `assets/*.md`: static English assistant instructions embedded with
  `include_str!`, matching the existing setup-asset style.
- `mod.rs`: CLI parsing, bootstrap, agent selection, and session launch.

The knowledge pack must be operational, not marketing copy. It should include
exact command names, file paths, trust boundaries, error codes, and source files.

### Product Model

The assistant must know:

- `pohunek` is a single-user, CLI-first, Rust daemon plus Rust CLI.
- The daemon owns PTYs, agent processes, sessions, metadata, worktrees, logs, and
  events.
- The CLI talks to a local Unix socket or a NetBird-bound TCP listener using
  newline-delimited JSON.
- Attach streaming uses a separate raw byte connection.
- Sessions run real PTY/TUI agents.
- Agent state comes from OSC titles, screen matching, PTY activity, and process
  state.
- Projects are per-host git repositories known to the daemon.
- `--project` is the normal remote-safe way to target a repo.
- No `--branch` means in-place. `--branch` means a managed worktree.
- Provider data stays caller-side.
- Host config is operator-trusted. Repo `.pohunek/` is repo-owned input.

### Configuration Reference

The assistant must know these config surfaces:

```text
~/.config/pohunek/
  launcher.conf
  prompts/*.tmpl
  templates.toml
  actions.toml
  agents/*.toml
  agents/manifests/*.toml
  hooks/*

<repo>/.pohunek/
  prompts/*.tmpl
  templates.toml
  actions.toml
  hooks/*
  setup
```

Rules:

- Host `agents/*.toml` can define program, args, env, input rules, resume, and
  manifest overrides.
- Repo `.pohunek/templates.toml` can name an agent profile but cannot define
  program, args, or env.
- Names use the single-segment name guard.
- Daemon reads use canonicalize-and-contain checks.
- Hooks execute code and must be reviewed like code.
- Profile env values are potentially secret.

### CLI Runbooks

The assistant must know how to use:

```bash
pohunek doctor --json
pohunek daemon start
pohunek health --json
pohunek setup
pohunek setup scripts
pohunek setup config
pohunek setup sway
pohunek integration install
pohunek host list --json
pohunek host inspect <host> --json
pohunek project add [path] --json
pohunek project list --json
pohunek project show <project> --json
pohunek project actions <project> --json
pohunek project action <project> <action> --json
pohunek session new ...
pohunek session list --json
pohunek session inspect <session> --json
pohunek session input <session> <text>
pohunek session stop <session>
```

It must know workflows for:

- local first setup;
- launcher setup and validation;
- Codex/Claude integration hook installation;
- NetBird and remote host checks;
- project registration;
- project action/prompt creation;
- host agent profile creation;
- hook creation and review;
- session launch and attach;
- update after a source or release change;
- general troubleshooting.

### Source Map

The assistant must be told where to verify details:

```text
docs/architecture.md
docs/phases/05-rofi-sway-launcher.md
docs/design/projects.md
docs/design/per-project-actions-and-worktree-hooks.md

crates/cli/src/main.rs
crates/cli/src/commands/setup.rs
crates/cli/src/commands/session.rs
crates/cli/src/commands/project.rs
crates/cli/src/commands/doctor.rs
crates/cli/src/paths.rs

crates/protocol/src/lib.rs
crates/protocol/src/session.rs
crates/protocol/src/project.rs
crates/protocol/src/capabilities.rs

crates/daemon/src/api/handler.rs
crates/daemon/src/session/mod.rs
crates/daemon/src/project/config.rs
crates/daemon/src/agent/profile.rs
crates/daemon/src/worktree/mod.rs
crates/daemon/src/capabilities.rs

scripts/lib.sh
scripts/pohunek-launch-issue
scripts/pohunek-launch-pr
scripts/pohunek-rofi
scripts/pohunek-rofi-issue
```

The static pack gives the model. The source map tells the agent where to verify
implementation details when precision matters.

## Live Snapshot

The assistant should start with a compact redacted snapshot.

Recommended sections:

```text
assistant:
  intent
  user_request
  selected_host
  selected_project
  selected_agent
  auto_started_daemon

paths:
  config_dir
  data_dir
  log_dir
  launcher_bin_dir
  sway_config_dir

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
```

Snapshot collection is best-effort. A failed snapshot item becomes a warning in
the snapshot, not a reason to lose all context.

### Redaction

The snapshot must not dump secrets into the prompt.

Rules:

- Do not include process environment variables.
- Do not include profile `[env]` values.
- Do not include hook script bodies by default.
- Do not include arbitrary config file bodies by default.
- Include filenames, existence, parse status, selected action names, and existing
  `--json` command output.
- Include prompt template content only when it is directly relevant to the
  selected project/action.

The assistant can later read specific files when useful. The initial snapshot
should stay conservative.

## Prompt Composition

The composed prompt should be deterministic and inspectable:

```text
# Pohunek Assistant

## Mission
You are the universal assistant for configuring, updating, troubleshooting, and
explaining pohunek.

## User Intent
intent: project
request: Configure the ui project launcher actions.

## Working Agreement
<safety.md>

## Pohunek Knowledge
<knowledge.md>

## Runbooks
<runbooks.md>

## Live Snapshot
<redacted snapshot>

## Source Map
<source-map.md>

## First Step
Summarize what you see, identify the next concrete action, and proceed using
documented pohunek commands and file edits. Verify changes before claiming they
work.
```

The prompt should explicitly tell the assistant that the source tree is
available and that it should verify uncertain details against the source map.

## Safety Model

The assistant is intentionally capable. It can inspect and edit files through the
underlying agent. The safety model is a working agreement plus existing
`pohunek` and filesystem controls.

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
- create or modify prompts, actions, templates, profiles, and hooks;
- run setup commands;
- start and inspect sessions;
- read relevant source and docs.

## Intent Behavior

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
`.pohunek/prompts/*.tmpl`, and project hooks when the user wants project-level
configuration.

### Update

The assistant helps reconcile installed setup assets, host config, project
definitions, and docs after a source or binary update.

It must avoid clobbering edited files. It should show diffs or explain file
changes before applying them.

### Debug

The assistant follows a debugging loop:

1. understand the failure;
2. collect structured state;
3. inspect relevant files;
4. form a concrete hypothesis;
5. test it;
6. apply the smallest fix;
7. verify.

### Help

The assistant answers directly from the knowledge pack, then uses the source map
when exact behavior matters.

## Data Flow

```text
user
  |
  | pohunek assistant <intent/request>
  v
CLI assistant command
  |
  | bootstrap daemon if local and needed
  | select agent
  | collect redacted snapshot
  | compose prompt
  v
session.new
  |
  | ordinary daemon request with initial input
  v
daemon-owned PTY session
  |
  v
selected agent profile
```

No special daemon-side assistant runtime is required. The feature reuses the
existing session lifecycle and control protocol.

## Output

Human output:

```text
started assistant session: local/s-123
agent: codex
intent: setup
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
- Snapshot item fails: include a warning and continue when possible.
- Prompt too large: fail before `session.new`; suggest `--no-snapshot` or a
  narrower project target.
- Initial input not confirmed: surface the existing warning and do not claim the
  assistant received its knowledge pack.

## Testing Strategy

### Unit Tests

- intent parsing maps every command form to the same internal launch model;
- agent selection is deterministic and visible;
- prompt composition is stable;
- `--print-prompt` never starts a session;
- snapshot redacts profile env values;
- daemon bootstrap policy is covered;
- prompt-size failures happen before daemon dialing.

### CLI Tests

- `pohunek assistant --print-prompt setup` prints the full prompt and exits;
- `pohunek assistant setup --json` starts or uses the daemon and sends
  `session.new` with composed `input`;
- remote assistant launch preserves `--yes` behavior;
- project intent includes selected project context;
- human output includes the attach command;
- JSON output includes assistant metadata.

### Snapshot Tests

- doctor report is embedded when available;
- host capabilities are embedded when available;
- selected project actions are embedded when available;
- config parse errors become snapshot warnings;
- profile env keys and values never appear in prompt output.

### Documentation Tests

- source-map paths exist;
- runbook commands match the CLI grammar;
- examples stay synchronized with parser tests.

## Definition of Done

The feature is complete only when the full assistant experience works:

- `pohunek assistant` launches a useful local assistant in one command.
- The command can bootstrap the local daemon when needed.
- Setup, project, update, debug, and help intents all work through the same
  assistant implementation.
- Agent selection is explicit in output and overrideable with `--agent`.
- The assistant prompt includes the knowledge pack, safety rules, source map,
  user intent, and redacted live snapshot.
- `--print-prompt` exposes the exact prompt without launching.
- Project intent includes project registration and action context.
- Remote launches preserve existing safety gates.
- Snapshot redaction prevents profile env values and process env values from
  entering prompts.
- Tests cover prompt composition, bootstrap, agent selection, snapshot redaction,
  local launch, remote launch, and project intent.

## Delivery Scope

This should land as one coherent feature, not as a narrow setup-only assistant
followed by a series of product expansions. Engineering can still split the work
internally across parser, prompt assets, snapshot collection, bootstrap, launch
wiring, and tests, but the shipped user-facing capability is the complete
universal assistant described here.

## Summary

The assistant should be one full-featured, universal agent. Intent only steers
the opening prompt; it does not split the system into separate assistants.

"Knows everything" means:

- curated product knowledge built into the CLI;
- redacted live context from the current host/project;
- source-map access to the repo;
- exact CLI runbooks;
- a safety model that keeps config and secret boundaries clear.

That gives the user a powerful one-command assistant while keeping `pohunek`
aligned with its existing architecture.
