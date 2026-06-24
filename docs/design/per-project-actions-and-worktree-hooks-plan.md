# Implementation Plan: Per-Project Actions, Worktree Hooks & Agent Profiles

Status: **proposed.** This is the companion implementation plan to the RFC
[`per-project-actions-and-worktree-hooks.md`](per-project-actions-and-worktree-hooks.md);
it turns the RFC's ten slices into ordered, testable milestones with concrete code
touch-points, mirroring how [`projects-plan.md`](projects-plan.md) followed
[`projects.md`](projects.md).

Companion to the RFC (the design + resolved decisions). Read the RFC first; this plan
assumes its resolved decisions verbatim — the daemon is the single source of truth,
config layers in-repo `.pohunek/` over host `~/.config/pohunek/`, there is **no trust
gate** (bounded by the A.5 safe subset and the hook env-clear), prompt resolution is
fail-closed, hooks are react-only, agent profiles *extend* a compiled base kind, and
`agent` becomes a free-string name on the wire. Every file:line below is cited against
the **current** source tree (not against RFC line numbers), and was re-verified while
writing this plan.

## Definition of Done

These are the top-level testable end-states; each becomes one or more milestone tests
below, and every RFC slice DoD bullet maps to a test here.

- **Slice 0:** the daemon resolves `$XDG_CONFIG_HOME/pohunek` (else `$HOME/.config/pohunek`)
  and **fails fast** when neither `XDG_CONFIG_HOME` nor `HOME` is set — no silent default.
- **Part A:** `pohunek [--host H] project {prompt,actions,action} <project> [<name>]` resolve
  daemon-side over the new `project.*` methods (human + `--json`); in-repo `.pohunek/`
  shadows the host layer per-name; a missing prompt is a hard `prompt_not_found`; the
  A.2.1 charset guard (`invalid_name`) and the symlink-containment guard fire **before any
  read** on all three read surfaces (`templates.toml`, `actions.toml`, `prompts/*.tmpl`) for
  **both** the in-repo and the host layer; an in-repo template/action may only **name** an
  agent and may never carry `program`/`argv`/`args`/`flags`/`env`; the two launchers thin
  into consumers of `project.action`, abort on `prompt_not_found` without starting a
  session, and `launcher.conf` shrinks to host-only keys with its flat parser unchanged.
- **Part B:** the single `.pohunek/setup` script generalizes into seven react-only events
  (`pre-create`/`post-create`/`pre-remove`/`post-remove`/`session-start`/`session-stop`/
  `agent-state`), all routed through one `run_hook` helper that `.env_clear()`s and sets an
  explicit allowlist (so no inherited `GITHUB_TOKEN`/`ANTHROPIC_API_KEY` and none of the
  `POHUNEK_*` handshake vars leak), bounds each by a timeout in its own process group, and
  discards stdout/stderr; `session-stop` fires from `record_exit` on every terminal reason
  with `POHUNEK_STOP_REASON`; `agent-state` fires on a real activity value change and never
  on a refresh tick, off the audit-log hot path; host-global then in-repo compose per event;
  a failing hook yields a non-fatal `SessionWarningKind::Hook`.
- **Part C:** `shell`/`codex`/`claude` become base kinds behind one `AgentAdapter` path
  (incl. a new `ShellAdapter`); the wire `agent` is a free string resolved daemon-side to a
  host profile (`~/.config/pohunek/agents/<name>.toml`) or a bare base kind, fail-closed
  (`agent_profile_not_found`); a profile extends one base kind with program/args/env/
  input-rules/resume/manifest overrides; the structural relaunch fields are snapshotted onto
  the session at creation and **never re-resolved** (profile `env` is **never** persisted and
  is re-resolved at resume); the whole `agents/` tree is owner-only + containment-gated; a
  profile body is never accepted over the wire.
- **No backward compatibility:** the breaking wire/on-disk/config changes ship with **no**
  shim or migration (see the dedicated section); `PROTOCOL_VERSION` stays `1`.
- All new logic is unit/integration tested; every milestone ends green
  (`cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`).

## Corrections to the design

The RFC cites struct/enum lines at their `#[derive]`/doc-comment line in several places.
The plan uses the verified keyword lines below; none of these change a decision, only the
edit anchor.

- **`Paths` has no `config_dir` and never reads `XDG_CONFIG_HOME`** — confirmed
  (`crates/daemon/src/paths.rs:50-66` reads only `XDG_RUNTIME_DIR`, `XDG_STATE_HOME`,
  `XDG_DATA_HOME`, `HOME`). The CLI is the template (`crates/cli/src/paths.rs`).
- **`SessionWarningKind`** `pub enum` keyword is `crates/protocol/src/session.rs:280`
  (`:278` is the `#[derive]`); `SetupScript` is `:288`.
- **`AgentKind`** `pub enum` keyword is `crates/protocol/src/session.rs:16` (RFC `:14`);
  `AgentActivity` is `:28`; `SessionNewParams` `:39`; `SessionListFilter` `:90` (`matches`
  impl `:106`); `SessionReportNativeIdParams` `:189`; `SessionInfo` `:309`.
- **`SessionRef` ctors:** `SessionRef::id` is `crates/daemon/src/agent/mod.rs:81` and carries
  the leading-dash argv-injection guard; `SessionRef::path` is `:116` and carries the
  must-be-absolute guard. (The drafts' `:100`/`:116` were approximate — `id` is at `:81`.)
  `SessionRefKind` enum is `:57`.
- **`persist_resume_binding` vs `resume_binding` (load-bearing):** the binding is *built*
  in `persist_resume_binding` (`crates/daemon/src/session/mod.rs:1349`, literal at
  `:1364-1376`); `resume_binding` (`:1448`, apply match `:1449-1461`) is the *apply* path.
  C.4's freeze logic lands in `persist_resume_binding`, not `resume_binding`.
- **`report_native_id`** is the fn at `crates/daemon/src/session/mod.rs:1288`; `:1294` is the
  unconditional `SessionRef::id(...)` call, `:1326` stores into `entry.info.native_session_id`.
  Today it **unconditionally** builds an `Id` ref and discards `params` transcript data —
  path-kind resume is genuinely new behavior, not a refactor.
- **`SessionRegistryConfig`** `pub struct` is `crates/daemon/src/session/mod.rs:126`
  (`:124` is the doc comment); `Default` impl is `:382-398` (the field list ends with
  `event_log_dir`/`detector_lag_warn_interval`; there is no `config_dir` yet).
- **`ResumeBinding`** `pub struct` is `crates/daemon/src/store/mod.rs:43` (`:42` is the
  `#[derive]`); `WorktreeBinding` is `:93`. The store doc header's "no secrets" note is at
  `:26-28`.
- **`WorktreeManager::new`** is *defined* at `crates/daemon/src/worktree/mod.rs:128` with
  signature `(root: PathBuf, store: Arc<Store>, setup_script_timeout: Duration)`; the sole
  call site is `crates/daemon/src/session/mod.rs:502`. `run_setup_script` is `:827`.
- **`DetectorConfig`** struct is `crates/daemon/src/detect/mod.rs:38`; the override seam is
  the `manifest: Option<Manifest>` field at `:40`, **not** the `include_str!` consts at
  `:33-35` nor the `OnceLock` getter `generic_shell_manifest()` at `:82`.
- **`StateMachine::tick`** — the behavior the agent-state dedup absorbs (the detector
  re-emits the same visible state every refresh) is correct; the dedup belongs to the
  agent-state hook dispatcher (Part B), not Part C.
- **A.2.1 guard vs `validate_git_ref_arg`:** the model `validate_git_ref_arg`
  (`crates/daemon/src/worktree/mod.rs:594`) only blocks empty/leading-`-`/control and returns
  `invalid_branch`, and is private. The A.2.1 single-segment guard is **stricter**
  (`^[A-Za-z0-9._-]+$`, no leading `.`/`-`, reject `/`/`\`/`..`) and uses the neutral
  `invalid_name` code — a **new** function, not a reuse.
- **Containment is a separate mandatory guard from the charset guard** (a charset-clean name
  can still be a symlink). Use real `std::fs::canonicalize` + `starts_with`, **not** the
  best-effort `canonical_or_original` (`crates/daemon/src/worktree/mod.rs:687`, which never
  fails) — containment must fail closed.

## New & changed types

### protocol (`crates/protocol`)

- `src/lib.rs` `mod method` — add three open-string consts after `PROJECT_REMOVE`
  (`lib.rs:108`): `PROJECT_ACTIONS = "project.actions"`, `PROJECT_ACTION = "project.action"`,
  `PROJECT_PROMPT = "project.prompt"`. Additive; **no `PROTOCOL_VERSION` bump**
  (`version.rs:19`). Extend the `pub use project::{…}` re-export (`lib.rs:42`) with every new
  `*Params`/`*Result`/`ActionSummary`/`ProviderKind`/`PromptLayer` type — the daemon/CLI
  cannot name them otherwise.
- `src/project.rs` (extend the existing file; do **not** create a new one):
  - `ProjectPromptParams { reference: String, name: String }` — `reference` mirrors
    `ProjectShowParams.reference` (`project.rs:108`); `name` is the required prompt name.
  - `ProjectPromptResult { name: String, content: String, layer: PromptLayer }` where
    `PromptLayer { InRepo, Host }` (`#[serde(rename_all="snake_case")]`).
  - `ProjectActionsParams { reference: String }`,
    `ProjectActionsResult { actions: Vec<ActionSummary> }`,
    `ActionSummary { name: String, provider: ProviderKind, template: String, layer: PromptLayer }`.
  - `ProjectActionParams { reference: String, name: String }`,
    `ProjectActionResult { provider: ProviderKind, agent: String, base_branch: Option<String>,
    branch: Option<String>, prompt_name: String, prompt_content: String }` — the recipe +
    resolved prompt content (A.3 / A.4). `agent` is a free string (Part C); the launcher
    passes it through verbatim.
  - `ProviderKind { LinearIssue, GithubPr, None }` (`#[serde(rename_all="snake_case")]` →
    `linear_issue`/`github_pr`/`none`).
  - Optional fields use `#[serde(default, skip_serializing_if = "Option::is_none")]`, matching
    the existing `ProjectInfo` fields (`project.rs:32`).
- `src/session.rs` (Part C wire flip — all `AgentKind` → `String`):
  - `SessionNewParams.agent` (`:39` struct; field) → `String` (required, no `serde(default)`).
  - `SessionInfo` (`:309`): `agent` → `String`, **plus a new** `pub agent_base: AgentKind`
    (the snapshotted resolved base kind, C.4) — a second field, not a rename.
  - `SessionListFilter::Agent(AgentKind)` (`:90`) → `Agent(String)`; the `matches()` arm
    (`:106`) becomes an OR-match: `session.agent == *name || base_kind_label(session.agent_base)
    == *name` (groups by the snapshotted base — C.4).
  - `SessionReportNativeIdParams.agent` (`:189`) → `String` (uniform migration; value is
    informational only — C.3).
  - `src/capabilities.rs` `AgentRuntime.agent` → `String`; `HostCapabilities.supported_agents`
    → `Vec<String>`.
  - `AgentKind {Shell,Codex,Claude}` (`:16`) **stays** as the internal base-kind type; keep its
    `pub use` re-export from `lib.rs`. **`IntegrationInstallParams.agent`/`IntegrationInstallReport.agent`
    stay `AgentKind`** (DECISION — the RFC is silent): hook install operates on base kinds
    (`install_claude`/`install_codex`), and the wire `agent` for install is a base kind. Keep
    the `use crate::session::AgentKind` import in `integration.rs`.
- `src/error.rs` — new `#[must_use]` constructors mirroring the one-per-code style at
  `error.rs:103-135`, all `ErrorClass::Runtime` (matching the existing `project_not_found`
  family): `prompt_not_found(name: &str)`, `action_not_found(name: &str)`,
  `template_not_found(name: &str)`, `invalid_name(kind: &str, name: &str)` (the **one shared**
  neutral bad-name code for prompt/action/template/agent/manifest — the message says which
  kind), `invalid_action(name: &str, reason: &str)`, `invalid_template(name: &str, reason: &str)`,
  `agent_profile_not_found(name: &str)`. Stable codes: `prompt_not_found`, `action_not_found`,
  `template_not_found`, `invalid_name`, `invalid_action`, `invalid_template`,
  `agent_profile_not_found`. The existing `agent_not_resumable` (`agent/mod.rs:235`) is reused
  for non-resumable profiles; `agent_binary_missing` (`agent/mod.rs:285`) for an unresolvable
  `program`.
- `PROTOCOL_VERSION` (`version.rs:19`) **stays `1`.** The `agent` enum→string flip is a
  genuinely breaking wire change, sanctioned by the no-back-compat stance (CLI+daemon are the
  same build), **not** by the additive policy in the `version.rs:14-18` comment. We
  deliberately do not bump — called out here so a reviewer does not flag it against that
  comment.

### daemon (`crates/daemon`)

- `src/paths.rs:28` — add `pub config_dir: PathBuf` to `struct Paths` (sibling of
  `log_dir`/`data_dir`); document it in the module-level "Resolved paths" list
  (`paths.rs:8-13`).
- `src/paths.rs:50` — `Paths::resolve()` derives it via the existing helper
  `xdg_or_home_relative("XDG_CONFIG_HOME", &[".config"])?` (`:90`) joined with `APP_DIR`
  (`:21`); add `config_dir,` to the returned literal. **No `dirs` crate** — match the CLI's
  `std::env` template.
- `src/session/mod.rs:126` — add `pub config_dir: Option<PathBuf>` to `SessionRegistryConfig`
  (after `event_log_dir`); `None` default in the `Default` impl (`:382-398`). `Option<PathBuf>`
  is `Eq` and keeps the ~20 `..SessionRegistryConfig::default()` test constructions compiling
  unchanged. Add a `pub fn config_dir(&self) -> Option<&Path>` accessor (next to the existing
  field accessors) as the **single read API** all three tracks use (decided in Slice 0).
- `src/session/mod.rs:163` — **generalize** the field `setup_script_timeout: Duration` to
  `hook_timeout: Duration` (default `DEFAULT_SETUP_SCRIPT_TIMEOUT` = 300s, `:394`). [Part B.]
- `src/session/mod.rs` — `SessionRegistryConfig` gains a loaded `ProfileRegistry` (or its
  source `agents_dir: Option<PathBuf>`, derived off `config_dir/agents`), wired from `main.rs`.
  Keep it `Clone`/`Eq`-safe. [Part C.]
- `src/main.rs:77` — the existing `SessionRegistryConfig` literal (already uses `..default()`)
  gains exactly the three new keys, **one per track and no duplicates**:
  `config_dir: Some(paths.config_dir.clone())` [Slice 0 owns this line],
  `hook_timeout: …` [Part B], and the profile-registry/`agents_dir` line derived from
  `paths.config_dir.join("agents")` [Part C].
- `src/project/config.rs` (new) — `ProjectConfigResolver { repo_root: PathBuf, config_dir: PathBuf }`
  owning all layered FS reads and both A.2.1 guards. **Not** part of `ProjectManager` (which
  stays store-glue, `project/mod.rs`); constructed per request from the resolved
  `ProjectRecord.repo_root` + the daemon `config_dir`. Public API:
  - `fn new(repo_root: PathBuf, config_dir: PathBuf) -> Self`
  - `fn resolve_prompt(&self, name: &str) -> Result<ResolvedPrompt, ProtocolError>` —
    `ResolvedPrompt { content: String, layer: PromptLayer }`.
  - `fn resolve_template(&self, name: &str) -> Result<ResolvedTemplate, ProtocolError>`.
  - `fn resolve_action(&self, name: &str) -> Result<ProjectActionResult, ProtocolError>`.
  - `fn list_actions(&self) -> Result<Vec<ActionSummary>, ProtocolError>`.
  - private `fn validate_name(kind: &str, name: &str) -> Result<(), ProtocolError>` (A.2.1.1)
    and `fn read_contained(&self, base: &Path, path: &Path) -> Result<String, ProtocolError>`
    (A.2.1.2). `read_contained` takes an explicit `base` so the **same** function enforces
    containment for the in-repo base (`<repo_root>/.pohunek/`) **and** the host base (`<config_dir>/`).
- `src/worktree/mod.rs` (Part B):
  - New `enum HookEvent { PreCreate, PostCreate, PreRemove, PostRemove, SessionStart,
    SessionStop, AgentState }` with `fn as_env(self) -> &'static str`
    (`pre-create`/…/`agent-state`).
  - New `struct HookContext` carrying the B.3.1 env fields (`session_id`, `project_id`,
    `agent`, `repo`, `worktree`, `branch`, `base_branch`, `stop_reason`, `activity` — each
    `Option`/empty per event) plus the resolved `cwd`.
  - New `fn run_hook(event: HookEvent, in_repo_dir: &Path, ctx: &HookContext, timeout: Duration,
    config_dir: &Path, warnings: &mut Vec<SessionWarning>)` — the generalization of
    `run_setup_script` (`:827`), reusing `wait_with_timeout` (`:904`), `terminate_setup_script`
    (`:928`), `SetupOutcome`, and the consts `SETUP_SCRIPT_INTERPRETER = "sh"` (`:55`) /
    `SETUP_SCRIPT_POLL_INTERVAL` (`:60`).
  - `struct WorktreeRequest` (`:68`) — add `pub agent: String` (resolved agent name).
  - `struct WorktreeManager` (`:111`) — generalize `setup_script_timeout: Duration` (`:121`)
    to `hook_timeout: Duration` and add `config_dir: PathBuf` (the host-global hooks root).
  - `WorktreeManager::new` (`:128`) — signature becomes
    `(root: PathBuf, store: Arc<Store>, hook_timeout: Duration, config_dir: PathBuf)`; the sole
    call site is `session/mod.rs:502`.
  - `cleanup_session` (`:322`, returns `Result<usize, ProtocolError>`) and `cleanup_project`
    (`:366`, returns `ProjectPrune`) — **widen** to thread `&mut Vec<SessionWarning>` so failing
    pre/post-remove hooks have an egress (DECISION, see Slice B1).
- `src/store/mod.rs:93` `WorktreeBinding` — add `pub agent: String` (the resolved agent
  **name**, persisted at bind time) so `POHUNEK_AGENT` is available to remove hooks
  (DECISION, see Slice B1). This is a **name only**, never a profile body/env — it stays within
  the store's no-secrets invariant. `#[serde(default, skip_serializing_if = "String::is_empty")]`
  to match the existing optional fields (`:67-72`).
- `src/store/mod.rs:43` `ResumeBinding` (Part C) — `agent: AgentKind` (`:47`) → `agent: String`;
  **add** `agent_base: AgentKind`, `program: String`, `args: Vec<String>`, `input_rules`,
  `resume_mode`, `ref_kind`, `resumable: bool`, each `#[serde(default, skip_serializing_if=…)]`
  like the existing `project_id`/`is_linked_worktree` (`:67-72`). **No `env` field — ever**
  (C.4 no-secrets invariant). Extend the doc header's non-secret enumeration (`:26-28`) to list
  these as the only added (non-secret) fields. Every added type must stay `Eq`.
- `src/agent/shell.rs` (new) — `ShellAdapter` implementing `AgentAdapter` (the C0 prerequisite).
- `src/agent/mod.rs:173` `AgentAdapter` — relax `fn id(&self) -> &'static str` (`:175`) → `&str`,
  `fn manifest(&self) -> &'static Manifest` (`:181`) → `&Manifest`, and the launch program in
  `launch_command` (`:186`) from `&'static str` to owned data. `build_pty_command` (`:199`) and
  `resolve_binary` (`:248`) already take `&str`.
- `src/agent/profile.rs` (new) — `AgentProfile` (parsed TOML) +
  `ResolvedProfile { name: String, base: AgentKind, program: String, args: Vec<String>,
  env: Vec<(String,String)>, input_rules: InputRules, manifest: Option<Manifest>,
  resume_template: ResumeTemplate, resumable: bool }` and a boot-time `ProfileRegistry`.
- `src/agent/mod.rs` — new `ResumeTemplate { mode: ResumeMode, ref_kind: SessionRefKind }`
  where `ResumeMode { Flag, Subcommand }`. `ref_kind` selects the validating ctor
  `SessionRef::id` (`:81`) vs `SessionRef::path` (`:116`).
- `src/detect/mod.rs:38` `DetectorConfig` — new constructor
  `for_profile(base: AgentKind, override_manifest: Option<Manifest>) -> Self` (sibling of
  `generic_shell()`/`codex()`/`claude()` at `:45/:62/:70`); uses the override `Manifest` when
  present, else the base kind's getter. **Host manifests go through `Manifest::parse_str`
  (`detect/manifest.rs:22`, returns `Result`, caps MAX_RULES=128/MAX_DEPTH=8/256 KiB), never the
  `.expect` getters.**
- `src/capabilities.rs:20` `host_capabilities` — replace the fixed `vec![Shell,Codex,Claude]`
  and the fixed 3-element `runtimes` vec with enumeration over loaded profile names + base kinds
  and a per-`program` probe via `which_on_path` (`:73`). **Unify or match the two `which`
  semantics:** `which_on_path` (`:73`, `.is_file()` only) vs `is_executable_file`
  (`agent/mod.rs:260`, checks `0o111`) so "available" agrees with what `resolve_binary` will
  launch.

### cli (`crates/cli`)

- `src/main.rs:129` `enum ProjectAction` — add `Prompt { reference, name, json }`,
  `Actions { reference, json }`, `Action { reference, name, json }` alongside
  `List/Add/Show/Rename/Rm`; add their arms to `wants_json` (`:397-403`) and the dispatch
  `match action` (`:645-675`), routing through `effective_host(&global_host, None)` (`:643`).
- `src/commands/project.rs` — `run_prompt`/`run_actions`/`run_action`, each mirroring `run_show`
  (`:124`): `request_with_params(method::PROJECT_*, &Params{…})`, `Client::connect(host, paths)`,
  deserialize the typed `*Result`, branch on `json`.
- `src/main.rs:283` `SessionAction::New` — drop `#[arg(long, value_enum, default_value = "shell")]`;
  `agent` becomes a plain `String` (default `"shell"`). [Part C.]
- `src/commands/session.rs` — remove `AgentArg` (ValueEnum) and `From<AgentArg> for AgentKind`
  (`:39-43`); `NewArgs.agent: String` (`:53`); `build_new_request` (`:421`) sends the string
  through. `agent_label(AgentKind)` (`:746`) → `agent_label(&str) -> &str` (identity);
  `parse_agent_filter` (`:195`) accepts any A.2.1-valid name. Add the `SessionWarningKind::Hook =>
  "hook"` arm to `warning_kind_label` (`:795`) — the match is exhaustive (no wildcard), so the
  build fails until added (the desired compile-time gate). [Part B + C.]

### scripts (`scripts/` + `crates/cli/src/commands/setup.rs`)

- `pohunek-launch-issue` / `pohunek-launch-pr` — replace the config reads of `agent`/`project`/
  template selection with one `pohunek [--host H] project action <project> <action>` call; keep
  `pohunek_render_provider_prompt` (`scripts/lib.sh:106`) and `pohunek_run_session_new` (`:175`)
  unchanged. Add a shared helper (e.g. `pohunek_launch_from_action`) in `scripts/lib.sh` that
  both launchers call, since they now differ only in the provider seam + template name.
- `scripts/lib.sh` — the flat `key=value` parser `pohunek_config_get` (`:30`) and the renderer
  (`:106`) are **unchanged**.
- `crates/cli/src/commands/setup.rs:73` `LAUNCHER_CONF` — **remove** the per-launch `agent`
  (`:78`) and `project` (`:84`) keys; keep host-only keys `host`/`terminal`/`list_timeout_seconds`/
  `linear_cli` and the commented optionals. Update the `next_steps` text (`:403`) to drop "set
  project". No Part-C-specific script change (a free-form `--agent <name>` is Part A; non-base-kind
  names only *require* Part C — see ordering).

## Milestones

### Slice 0 — Daemon config dir (shared prerequisite)

The daemon resolves socket/lock/log/data dirs from XDG base dirs (`paths.rs:50`) but has **no
config dir** — only the CLI does. Parts A (host-default `~/.config/pohunek/`), B (host-global
`~/.config/pohunek/hooks/<event>`), and C (`~/.config/pohunek/agents/*`) all need it. This slice
is a near-exact port of the CLI's resolution into the daemon: add a fail-fast `config_dir` to
`Paths`, expose it on `SessionRegistryConfig` (with a `config_dir()` accessor), and wire it from
`main.rs`. It adds **no consumer** — it only makes the value available, so it ships independently
and unblocks A1, B3, and C1.

**Touch-points & exact changes.**
1. `paths.rs:28` — add the `config_dir: PathBuf` field; doc it in the "Resolved paths" list
   (`:8-13`).
2. `paths.rs:50` — after the data-dir block (`:66`), add (reusing the helper verbatim, exactly
   as `log_dir`/`data_dir` do):
   ```rust
   // Config dir: prefer XDG_CONFIG_HOME, else ~/.config. One of the two must
   // resolve; otherwise fail fast (DaemonError::MissingEnv).
   let config_home = xdg_or_home_relative("XDG_CONFIG_HOME", &[".config"])?;
   let config_dir = config_home.join(APP_DIR);
   ```
   and add `config_dir,` to the returned `Ok(Self { … })`.
3. `session/mod.rs:126` — add `pub config_dir: Option<PathBuf>` after `event_log_dir`, with
   `config_dir: None,` in the `Default` impl (`:382-398`); add the `config_dir()` accessor.
4. `main.rs:77` — add `config_dir: Some(paths.config_dir.clone()),` to the struct literal.

**Read API (decided here, referenced by A1/B3/C1).** Consumers read the config dir through the
**single** `SessionRegistryConfig::config_dir()` accessor. A1's handlers read it off
`state.sessions.config_dir()`; B3 passes `config.config_dir` into `WorktreeManager::new`; C1
derives `agents_dir` from `config.config_dir.join("agents")`. No track re-reads env or
re-derives the path independently.

**Fail-fast guarantee.** No new error path: `xdg_or_home_relative("XDG_CONFIG_HOME", …)` already
returns `DaemonError::MissingEnv { var: "XDG_CONFIG_HOME or HOME" }` (`paths.rs:96`) when neither
is set/non-empty, which bubbles `Paths::resolve()` → `run()` → `main()`, printed via `eprintln!`
+ `ExitCode::FAILURE`. Reuses the established typed-error idiom (`DaemonError::MissingEnv`,
`error.rs:19`). The CLI additionally rejects an empty `HOME`; the daemon's `require_env` already
rejects empty values (`paths.rs:82`), so behavior is equivalent. **No new error variant; no
silent default.**

**Tests** (each RFC Slice-0 DoD bullet as an assertion). Add `#[cfg(test)] mod tests` to
`paths.rs`. Env-var resolution is process-global and racy under the parallel runner, so guard it
with a dedicated `static ENV_LOCK: Mutex<()>` with restore-on-drop, mirroring the in-repo
precedent (`crates/daemon/tests/health_socket.rs:49,64,77,96`). Set every XDG var the resolver
reads (`XDG_RUNTIME_DIR`, `XDG_STATE_HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `HOME`) to temp
paths so the test is hermetic:
- `config_dir` from `XDG_CONFIG_HOME` — set `XDG_CONFIG_HOME=<tmp>/cfg`, assert
  `Paths::resolve()?.config_dir == <tmp>/cfg/pohunek`.
- `config_dir` falls back to `$HOME/.config` — unset `XDG_CONFIG_HOME`, set `HOME=<tmp>/home`,
  assert `config_dir == <tmp>/home/.config/pohunek`.
- fails fast when neither is set — unset both, assert
  `matches!(Paths::resolve(), Err(DaemonError::MissingEnv { var }) if var == "XDG_CONFIG_HOME or HOME")`.
- default config is `None` — `assert_eq!(SessionRegistryConfig::default().config_dir, None)`,
  pinning that existing `..default()` constructions stay valid.

Anchor for a fake `Paths` **without** mutating env (prefer where a test only needs a populated
struct): `crates/cli/src/commands/doctor.rs:349-365` fills every field from a tmp base dir; a
daemon-side equivalent should add `config_dir: base.join("config").join("pohunek")`.

**Checkpoint.** `cargo test -p pohunek-daemon paths` green. A daemon launched with
`XDG_CONFIG_HOME=/some/dir` makes `SessionRegistryConfig.config_dir == Some("/some/dir/pohunek")`
reach `SessionRegistry::new`; unsetting both `XDG_CONFIG_HOME` and `HOME` makes the daemon exit
non-zero with the `MissingEnv` message instead of inventing a path. `cargo clippy --all-targets`
and `cargo fmt --check` clean.

---

### Slice A1 — `project.prompt` resolution + CLI (Part A core)

Build the `ProjectConfigResolver` and the prompt primitive end to end. Depends on Slice 0.

**`resolve_prompt` order** (fail-closed, **first-existing-file wins, no merge**): (1)
`<repo_root>/.pohunek/prompts/<name>.tmpl`; (2) `<config_dir>/prompts/<name>.tmpl`; (3) else
`Err(prompt_not_found(name))`. **No silent fallback** to a built-in.

**`validate_name(kind, name)`** (A.2.1.1, run **before any join/read**, on both the wire `<name>`
and any in-repo `prompt=` value): reject unless `name` matches `^[A-Za-z0-9._-]+$`, is non-empty,
and does not begin with `.` or `-`; any `/`, `\`, `..`, or control char → `invalid_name(kind, name)`.
This is a **new, stricter** guard than `validate_git_ref_arg` (`worktree/mod.rs:594`) — mirror its
shape, do not import it.

**`read_contained(base, path)`** (A.2.1.2, run for **every** read — both TOMLs and prompts, both
layers): `std::fs::canonicalize(path)` (a non-existent path is a not-found, return the caller's
typed not-found, not a guard failure), then assert the canonicalized path `starts_with` the
**canonicalized** `base` (`<repo_root>/.pohunek/` resp. `<config_dir>/`); a symlink that escapes →
typed error (a dedicated containment message, kept typed, never a panic). Use real
`std::fs::canonicalize` + `starts_with`, **not** `canonical_or_original` (`worktree/mod.rs:687`),
because containment must fail closed. `base` is explicit so the host layer is guarded with
`base=<config_dir>` and the repo layer with `base=<repo_root>/.pohunek/`.

**Protocol/handler.** Add `PROJECT_PROMPT` const, `ProjectPromptParams`/`ProjectPromptResult`/
`PromptLayer` (`project.rs`), re-export (`lib.rs:42`), and `prompt_not_found` + `invalid_name`
constructors (`error.rs`). Handler `handle_project_prompt` in `api/handler.rs` (new arm in the
dispatch `match`, after `PROJECT_REMOVE`), mirroring `handle_project_show`:
`parse_params::<ProjectPromptParams>` → `require_projects` → capture `reference`/`name`/`config_dir`
(off `state.sessions.config_dir()`) → `run_project_blocking` whose closure does `pm.resolve(&reference)?`
(read-only — do **not** `touch`), builds `ProjectConfigResolver::new(record.repo_root, config_dir)`,
calls `resolve_prompt(&name)`. All FS work runs inside the `spawn_blocking` closure.

**CLI.** `ProjectAction::Prompt { reference, name, json }` (`main.rs:129`) + `run_prompt`
(`commands/project.rs`, mirroring `run_show`).

**Tests** (every A1 DoD bullet, real temp repos + temp config dir, reusing the
`init_repo`/`git_in`/`manager`/`unique_dir` harness at `project/mod.rs:511-823`):
- in-repo `prompts/<name>.tmpl` **wins** over an identically-named host-default prompt (assert
  `content` + `layer == InRepo`);
- a missing name → `prompt_not_found` (assert `err.code`, mirroring `project/mod.rs:691-710`);
- a traversal name (`"../../../../etc/passwd"`, `"a/b"`, `"-leading"`, `".hidden"`, a control char)
  → `invalid_name`;
- **symlink-escape, all three read surfaces, BOTH layers:** a charset-clean `prompts/x.tmpl`, a
  `templates.toml`, **and** an `actions.toml` symlinked outside `.pohunek/` (e.g. → `/etc/hostname`)
  are each **rejected** for the repo layer (`base=<repo_root>/.pohunek/`); and a charset-clean
  `<config_dir>/prompts/x.tmpl`, `<config_dir>/templates.toml`, `<config_dir>/actions.toml`
  symlinked outside `<config_dir>` are each **rejected** for the host layer (`base=<config_dir>`).
  Add direct unit tests against `read_contained` for the TOML paths (this slice wires containment
  into the read path even though only `resolve_prompt` is exercised by name — the guard is shared);
- **remote project over `--host`** — a daemon-level test that the resolver works against a
  `repo_root` regardless of caller locality (the daemon reads its own host's `.pohunek/`);
- `--json` shape covered (json round-trip + CLI render).

**Checkpoint.** `pohunek [--host H] project prompt <project> <name>` prints the resolved template
(or a typed `prompt_not_found`); a repo committing `prompts/issue.tmpl` shadows the host default; a
symlinked `prompts/evil.tmpl → /etc/passwd` is refused, in both layers.

---

### Slice A2 — `project.action` / `project.actions` + templates (Part A)

Add template + action resolution and recipe assembly on top of A1's resolver and guards.

**`resolve_template(name)`:** read `<repo_root>/.pohunek/templates.toml` then
`<config_dir>/templates.toml` through `read_contained`, parse with `toml::from_str` into a
`HashMap<String, RawTemplate>`; **per-name, most-specific layer wins whole** (in-repo
`[template.X]` shadows host `[template.X]` entirely — no field-merge); the *set* of names is the
**union**. `RawTemplate { agent: String (required), prompt: String (required), base_branch:
Option<String> }` with `#[serde(deny_unknown_fields)]` so an unknown key → `invalid_template(name,
reason)`. A missing `base_branch` falls through to the project default → repo HEAD — the **one
scalar fallthrough**, passed as `None` to the worktree layer (matching the existing
`default_base_branch` chain at `session/mod.rs:952`; **do not** re-implement HEAD resolution here).

**`resolve_action(name)`:** same two-layer union/shadow over `actions.toml`.
`RawAction { template: String (required), provider: ProviderKind (required), branch: Option<String> }`
with `deny_unknown_fields` → `invalid_action`. **A.5 enforcement is structural:** because
`RawTemplate`/`RawAction` only carry the safe fields, an in-repo definition attempting
`program`/`argv`/`args`/`flags`/`env` is rejected by `deny_unknown_fields` as
`invalid_template`/`invalid_action` — there is no field to set it. **A.4 rule:** if `provider !=
None` and `branch` is `Some` → `invalid_action` (the branch comes from the provider). Resolve
`template` → if it resolves nowhere, `template_not_found`; resolve the template's `prompt` via
`resolve_prompt` (so a recipe always carries `prompt_content` + `prompt_name`). Assemble
`ProjectActionResult { provider, agent, base_branch, branch, prompt_name, prompt_content }`. **Run
`validate_name` on every name** that becomes a path segment — the action name, the template name it
references, and the template's `prompt=` value — before the corresponding read.

**`list_actions()`:** union of action names across layers → `Vec<ActionSummary { name, provider,
template, layer }>`, each name `validate_name`-checked.

**Protocol/handlers.** `PROJECT_ACTION`/`PROJECT_ACTIONS` consts; `ProjectActionParams`/`ProjectActionResult`,
`ProjectActionsParams`/`ProjectActionsResult`/`ActionSummary`, `ProviderKind`; re-exports;
`action_not_found`/`template_not_found`/`invalid_action`/`invalid_template` constructors. Handlers
`handle_project_action`/`handle_project_actions` (new dispatch arms), same `require_projects →
run_project_blocking → resolve_*` shape as A1. CLI `ProjectAction::Action`/`Actions` +
`run_action`/`run_actions`.

**Tests** (every A2 DoD bullet; reuse the `project/mod.rs:511-823` repo harness, writing real
`.pohunek/{templates,actions}.toml` + `prompts/*.tmpl`):
- an action resolves to a recipe **carrying the resolved prompt content** (assert `prompt_content`
  is the `.tmpl` body and `provider`/`agent`/`base_branch` match the template);
- an in-repo template selecting `codex` is honored (assert `agent == "codex"` flows through);
- **A.5 — full forbidden-key set:** iterate `{program, argv, args, flags, env}` against both
  `[template.X]` and `[action.X]`, asserting each is independently rejected — a `[template.x]
  program = …` AND a `[template.x]` `env`/`[env]` table both yield `invalid_template`, and an
  action-level disallowed key yields `invalid_action` (pins A.5's "arbitrary argv/flags" clause,
  not just `program`/`env`);
- an action with an explicit `branch` under a non-`none` provider → `invalid_action`;
- `template_not_found` when an action names a template resolving nowhere; `action_not_found` for an
  unknown action name; `invalid_name` for a bad template/prompt name inside the TOML;
- `project actions` lists the **union** of resolvable actions with the template each uses, in-repo
  names shadowing host names of the same key.
Mirror the bad-input iteration style at `project/mod.rs:691-710` and the json round-trip pattern.

**Checkpoint.** `pohunek project actions <project>` lists actions with their templates; `pohunek
project action <project> <name>` returns the full recipe + prompt content; a repo trying to inject
a `program`/`env` key is rejected with `invalid_template`.

---

### Slice A3 — Launcher consumes the daemon (Part A)

Thin `pohunek-launch-issue`/`-pr` into consumers of `project.action`. Each launcher: (1) call
`pohunek [--host H] project action <project> <action>` → JSON `{provider, agent, base_branch,
branch, prompt_name, prompt_content}`; (2) **materialize `prompt_content` to a temp file** (the
renderer `pohunek_render_provider_prompt` at `lib.sh:106` reads a template **path**, not a string —
use `mktemp` + cleanup trap as `pohunek-rofi-issue:46-47` does); (3) fetch provider data with the
caller's own creds (`linear`/`gh`); (4) render via the **unchanged** renderer; (5) derive the
branch from the provider JSON (`branchName`/`headRefName`); (6) `pohunek_run_session_new` (`lib.sh:175`)
with `--agent <recipe.agent> --project <project> --branch <derived> --input <rendered>`. The
launcher learns `<project>` + `<action>` as launcher args/flags (they left the config); the daemon
resolves `<project>` on the target host (consistent with RFC Decision 1).

`launcher.conf` (`setup.rs:73`): remove `agent`/`project`; keep flat `key=value` host-only keys;
parser (`lib.sh:30`) unchanged. **No client-side TOML.**

**Tests** (every A3 DoD bullet, reworking the launcher integration tests at
`crates/cli/tests/scripts.rs:78` `launch_pr_*` and `:156` `launch_issue_*`). The mock `pohunek`
(`scripts.rs:95-100`) becomes a **two-mode stub**: it must **answer** `project action <project>
<action>` with a recipe JSON **and** log `session new` argv:
- **two projects launch issues with different agents/prompts** driven entirely by daemon-resolved
  definitions (new variant proving per-project agent divergence — the mock returns `agent=claude`
  for one, `agent=codex` for the other; assert the `session new --agent` argv differs);
- **prompt-missing aborts cleanly:** the mock `project action` returns a typed `prompt_not_found`
  error and the launcher **exits non-zero WITHOUT logging any `session new` argv** (no silent
  fallback — RFC edge case);
- the **flat-`launcher.conf` parser still reads the remaining host-only keys** (assert
  `linear_cli`/`terminal`/`list_timeout_seconds` still drive the launcher);
- the renderer's **`${var}`/tab-flatten guards still hold** — keep the rendered-prompt content
  assertions (`scripts.rs:145`/`:215`) and the rofi-row tab-flatten assertions (`scripts.rs:307-308`);
  **add an explicit unknown-`${var}` rejection test** at the script layer (daemon-served templates
  can now carry arbitrary content);
- the existing **no-token-leak** assertions stay (no `--repo`, no `GITHUB_TOKEN`/`LINEAR` token in
  the daemon-bound argv). The `install_config_creates_then_skips_then_force_rewrites` byte-equality
  test (`setup.rs:566`) self-updates from the `LAUNCHER_CONF` const; update the `next_steps` text
  test if one asserts the removed `project` line.

**Checkpoint.** With `agent`/`project` gone from `launcher.conf`, two repos with different
`.pohunek/templates.toml` launch issue sessions with different agents and prompts; a
`prompt_not_found` aborts before `session new`; the only client config left is host-only keys; the
renderer rejects an unknown `${var}` in a daemon-served template.

---

### Slice B1 — Generalize the setup script into hooks (Part B core)

Replace `run_setup_script` with `run_hook`, wire the four worktree-lifecycle events, and add the
`SessionWarningKind::Hook` variant. This is the slice that closes the env-inheritance security gap:
today the lone setup script (`worktree/mod.rs:827`) spawns with the daemon's **full inherited
environment** (it sets only arg/cwd/stdio/process-group, never `.env_clear()`), so a hostile in-repo
hook can exfiltrate `GITHUB_TOKEN`/`ANTHROPIC_API_KEY`/`POHUNEK_SOCKET_PATH` via its own outbound
`curl` (the `/dev/null` redirection hides only *output*).

**Two decisions resolved here** (the RFC left them as gaps; they are prerequisites for B1's own DoD,
not deferrable):
- **`POHUNEK_AGENT` for remove hooks.** `WorktreeBinding` (`store/mod.rs:93`) has no `agent` field
  today, so a remove hook cannot otherwise set `POHUNEK_AGENT`. **Decision:** add `agent: String`
  (the resolved agent **name** — non-secret, never a profile body/env) to `WorktreeBinding`,
  persisted at bind time and read in `cleanup_session`/`cleanup_project`. This keeps the B.3.1
  contract intact (`POHUNEK_AGENT` present for all events) and stays within the store's no-secrets
  invariant.
- **Remove-hook warning egress.** `cleanup_session` returns `Result<usize, ProtocolError>` (`:322`)
  and `cleanup_project` returns `ProjectPrune` (`:366`); neither carries a `Vec<SessionWarning>`.
  **Decision:** widen both to thread `&mut Vec<SessionWarning>` (touching their callers in
  `session/mod.rs`) so a failing pre/post-remove hook surfaces a `SessionWarningKind::Hook` warning,
  matching the create-hook contract (`bind` threads `&mut warnings` into `WorktreeBound.warnings`).

**Touch-points & exact changes.**
- `crates/protocol/src/session.rs:288` — add `Hook` (**unit** variant) to `SessionWarningKind`
  after `SetupScript`. The enum derives `Copy` and `#[serde(rename_all="snake_case")]`, so `Hook`
  serializes to the bare string `"hook"`. It MUST be a unit variant — a struct variant
  `Hook { event }` would break the `Copy` derive and the bare-string shape. The failing event name
  rides in `SessionWarning.message`/`detail`, not the kind.
- `crates/cli/src/commands/session.rs:795` — add `Hook => "hook"` to `warning_kind_label`.
- `crates/daemon/src/worktree/mod.rs`:
  - Lift the spawn machinery out of `run_setup_script` (`:827`) into `run_hook`. **Reuse verbatim:**
    `.stdin/.stdout/.stderr(Stdio::null())`, `#[cfg(unix)] builder.process_group(0)`,
    `wait_with_timeout` (`:904`), `terminate_setup_script` (`:928`), `SetupOutcome`. **Net-new
    (load-bearing):** `builder.env_clear()` followed by explicit `.env(k, v)` for `PATH`, `HOME`,
    and the `POHUNEK_*` context vars from `HookContext` (B.3.1). Invocation stays `sh <script>`
    (interpreter `:55`) so a non-executable committed hook still runs. **Do NOT** route hooks
    through `run_command`/`git_run`: that executor is unbounded and captures output — the opposite
    of hook discipline.
  - **post-create back-compat / matrix:** the legacy `run_setup_script(&path, …)` call at `:269`
    becomes `run_hook(HookEvent::PostCreate, &path, …)`. The in-repo post-create slot =
    `.pohunek/hooks/post-create` if present, **else** `.pohunek/setup` (`SETUP_SCRIPT_REL`, `:51`)
    — never both. (Host-global composition is B3.)
  - **Fresh-create guard:** `pre-create`/`post-create` fire only on the fresh-create path. The reuse
    early-return is at `:216` (and the foreign-conflict return at `:239`), both before the create
    seam. Place `pre-create` after the base-branch resolution + fetch (so `POHUNEK_BASE_BRANCH` is
    available); place `post-create` at the existing seam (`:269`).
  - **Remove seams:** in `cleanup_session` (`:322`) fire `pre-remove` just before `worktree_remove`
    (`:333`) and `post-remove` after `removed += 1` (`:341`); in `cleanup_project` (`:366`) fire
    `pre-remove` before `worktree_remove` (`:388`) and `post-remove` at `prune.removed += 1`
    (`:399`). Skipped (live-session) worktrees `continue` (`:385`) and MUST NOT fire remove hooks.
    `post-remove` cwd = `binding.repository`; if it no longer exists, skip the hook with a `Hook`
    warning (an `exists()` check).
  - Both create and remove hooks build `HookContext` from in-scope data: create from
    `req`/`repository`/`path`/`base_branch`/`req.agent`; remove from the `WorktreeBinding`
    (`binding.session_id`, `.project_id`, `.repository`, `.path`, `.branch`, `.base_branch`, **and
    the new `.agent`**). `POHUNEK_BASE_BRANCH` **is** on the binding, so it is available for remove
    hooks too.
- `crates/daemon/src/store/mod.rs:93` — add `agent: String` to `WorktreeBinding` (serde-default).
- `crates/daemon/src/session/mod.rs` — add `agent: String` to the `WorktreeRequest` literal in
  `bind_worktree` (`:1086`), sourced from `params.agent` at `resolve_target` (`:905`), and persist
  it onto the `WorktreeBinding`. **Because Part C lands before Part B (see Build order), `params.agent`
  is already a `String`** and no `AgentKind`→label shim is written.

**New signatures.**
```rust
pub enum HookEvent { PreCreate, PostCreate, PreRemove, PostRemove, SessionStart, SessionStop, AgentState }
impl HookEvent { fn as_env(self) -> &'static str { /* "pre-create" … "agent-state" */ } }

struct HookContext {
    cwd: PathBuf,
    session_id: String,
    project_id: Option<String>,   // POHUNEK_PROJECT_ID empty if None
    agent: String,
    repo: Option<PathBuf>,        // create/remove
    worktree: Option<PathBuf>,    // post-create, pre-remove
    branch: Option<String>,       // create/remove
    base_branch: Option<String>,  // post-create (+ remove from binding)
    stop_reason: Option<&'static str>, // session-stop
    activity: Option<&'static str>,    // agent-state
}

fn run_hook(event: HookEvent, in_repo_dir: &Path, ctx: &HookContext, timeout: Duration,
            config_dir: &Path, warnings: &mut Vec<SessionWarning>);
```

**Tests** (every B1 DoD bullet; mirror the run-setup-script suite in
`crates/daemon/src/worktree/tests.rs`, extending `manager()`/`manager_with_timeout()` (`:87-95`) +
`request()` (`:113`) to pass `config_dir`, `hook_timeout`, and `WorktreeRequest.agent`):
- a repo with `.pohunek/hooks/post-create` runs it on worktree create; on failure a
  `SessionWarningKind::Hook` warning is present and the worktree is kept (mirror
  `failing_setup_script_keeps_the_worktree_with_a_warning`, `tests.rs:524`); a passing hook produces
  no warning (mirror `successful_setup_script_produces_no_warning`, `:554`);
- `pre-remove` runs *before* `git worktree remove`; `post-remove` cwd is `binding.repository`
  (anchor off `cleanup_session_removes_only_owned_worktrees` `:755` and
  `cleanup_project_removes_only_the_projects_owned_worktrees` `:783`; assert hooks do NOT fire on
  skipped live worktrees);
- a failing **pre/post-remove** hook surfaces a `Hook` warning through the widened
  `cleanup_session`/`cleanup_project` egress, and removal still proceeds (best-effort);
- the **B.1 setup-vs-post-create matrix** rows that do not involve the host layer hold (host rows
  deferred to B3): `repo post-create only` → repo post-create runs; `repo setup only` → setup runs;
  `both` → post-create runs, setup ignored (never both);
- **`.env_clear` test (new — no existing coverage):** set a sentinel `GITHUB_TOKEN` on the daemon
  process, have the hook write its own environment / `$GITHUB_TOKEN` to a file in the worktree,
  assert the sentinel is **absent** (pairs with `failing_setup_script_warning_detail_excludes_script_stderr`,
  `tests.rs:572`, which covers output but NOT env leakage);
- **hook env carries the exact B.3.1 var set per event:** for each of
  `pre/post-create`/`pre/post-remove`, a hook that dumps `env` proves exactly
  `POHUNEK_HOOK_EVENT`/`_SESSION_ID`/`_PROJECT_ID`/`_AGENT`/`_REPO`/`_BRANCH` (+`_WORKTREE`/`_BASE_BRANCH`
  where the table marks them) plus `PATH`/`HOME`, and that none of the handshake vars
  (`POHUNEK_SOCKET_PATH`/`_DAEMON_ID`/`_ENV`/`_PROTOCOL_VERSION`) are present. `POHUNEK_AGENT` is
  asserted **present** for pre/post-remove (sourced from the persisted `WorktreeBinding.agent`);
- output never appears in the event log (mirror `tests.rs:572`);
- timeout still kills the whole process subtree (mirror
  `hanging_setup_script_is_terminated_with_its_forked_children`, `tests.rs:609`, with its
  `read_setup_child_pid`/`wait_until_process_gone` helpers);
- **worktree reuse:** reusing an existing worktree (hit the `:216` early-return) fires **NO**
  pre/post-create hook (RFC edge case 820);
- **launch-failure rollback:** `post-create` fires then the binding persist fails and the worktree
  is rolled back via `worktree_remove` (`:290`) **without** firing any remove hook — anchor off
  `binding_persist_failure_rolls_back_the_worktree` (`tests.rs:706`); v1 requires `post-create`
  effects to be idempotent (asserted by the no-remove-hook-on-rollback test);
- `WorktreeRequest.agent` round-trips into `POHUNEK_AGENT` and onto `WorktreeBinding.agent`;
- protocol roundtrip: `SessionWarningKind::Hook` serializes to `"hook"` (extend
  `session_warning_json_shape_roundtrips`, `crates/protocol/tests/roundtrip.rs:510`).

**Checkpoint.** `cargo test -p pohunek-daemon worktree` and `-p pohunek-protocol` green; committing
`.pohunek/hooks/post-create` that touches a file and launching a `--branch` session creates the file
in the worktree; a hook that runs `env > /tmp/x` shows no `GITHUB_TOKEN`; a failing hook surfaces a
`"hook"` warning while the worktree survives; reusing a worktree fires no create hook.

---

### Slice B2 — Session-lifecycle & agent-state hooks (Part B)

Add the three non-worktree events: `session-start` (after PTY spawn), `session-stop` (every terminal
exit, with `POHUNEK_STOP_REASON`), and `agent-state` (on activity-value change, off the audit-log
hot path). All three route through `run_hook` from B1.

**Touch-points & exact changes (`crates/daemon/src/session/mod.rs`).**
- **`session-start`:** fire in `create()` right after `let info = launch?;` (`:786`), BEFORE the
  initial-input injection. cwd = `info.cwd`; ctx = session id + agent. **Do NOT** fire in
  `register_pty_session` (`:1101`) — it is shared with `resume_binding` (`:1448`) and would re-fire
  `session-start` on every daemon-restart resume. Use the bounded/best-effort discipline already
  applied around `await_initial_input_readiness` so a hung hook cannot wedge `session.new`.
- **`session-stop`:** fire from INSIDE `record_exit` (`:1980`), in/after the terminal state-assignment
  block (`:2000-2006`), because the reason is only known there. Derive `POHUNEK_STOP_REASON` from the
  same branch: `stopped` → `"stopped"`, `exit.success` → `"done"`, else `"failed"`. cwd =
  `entry.info.cwd`. **Critical:** do NOT key the hook off the `SESSION_STOPPED` event — `Done`/`Failed`
  emit `SESSION_UPDATED` (`:2015`), so an event-subscription dispatcher would miss natural exits. Fire
  it on any terminal transition. The work must NOT run under the `sessions` `Mutex` (the hottest lock)
  — spawn/bound it after the lock is dropped (alongside the existing post-lock `persist_resume_binding`/`emit`
  at `:2027-2028`). `record_exit` never calls `cleanup_session`, so `session-stop` correctly fires
  with the worktree retained and no remove hook.
- **`agent-state` dispatcher:** spawn a dedicated task off `self.subscribe()` (`:536`), mirroring
  `events::spawn_drain` (`events/mod.rs:114`) exactly — `biased` `tokio::select!` on a
  `CancellationToken` vs `events.recv()`, with explicit `RecvError::Lagged`/`Closed` handling so a
  slow hook cannot wedge the broadcast. Filter `event.event == protocol::event::AGENT_STATE` and read
  `session_id`/`activity` from the inline JSON payload built in `record_activity` (`:1951`). Maintain a
  `HashMap<SessionId, AgentActivity>` of the **last-fired activity value** and fire only on an actual
  change — `record_activity` does NOT dedup, and the detector republishes the same visible state every
  refresh tick (`StateMachine::tick`, driven through `Detector::tick`), so a naive subscriber fires
  every ~800ms. Dedup on the activity **value** only (working/blocked/idle). Add a short time-debounce
  on top to smooth genuine flap. On `Lagged`, re-read the session's current activity and compare against
  last-fired (do NOT blindly reset to `None`, which would double-fire). The dedup lives in the
  **dispatcher**, never in `record_activity` — moving it there would suppress `AGENT_STATE` for the
  event log and CLI status. Store the dispatcher's `JoinHandle` + `CancellationToken` on the inner
  struct and add a `shutdown_*` awaiter, mirroring `spawn_event_log` (`:661`) / `shutdown_event_log`
  (`:685`).

**Tests** (every B2 DoD bullet):
- `session-start` runs after spawn (anchor a daemon-level test off the `create` path; assert the hook
  ran and the create round-trip still returned);
- `session-stop` fires **once per terminal exit** with the correct reason: `stopped` on the `stop()`
  path (mirror `stop_marks_running_session_stopped`, `:3011`), `done` on the natural-exit watcher
  (mirror `detects_successful_process_exit`, `:2993`), and a `failed` variant for a non-zero exit.
  Confirm it does NOT interfere with the resume-binding removal ordering
  (`stopping_a_session_drops_its_resume_binding` `:3416`, `resize_then_stop_leaves_no_binding` `:3686`);
- **agent-state transitions cover all of working/blocked/idle:** `working → blocked`, `working → idle`,
  and `idle → working` each fire exactly **once**, and a same-state refresh tick fires **none** (drive
  the over-fire scenario proven by
  `stable_visible_refresh_reemits_same_visible_non_process_state_after_interval` /
  `tick_delegates_stable_visible_refresh`). **Pin the contract** that `POHUNEK_ACTIVITY` only ever
  carries `working`/`blocked`/`idle` — no `done`/terminal value — referencing `AgentActivity`
  (`protocol/src/session.rs:28`);
- a slow `agent-state` hook cannot stall the event log: under a small-capacity broadcast the dispatcher
  survives `Lagged` (mirror `drain_keeps_running_after_a_slow_consumer_lag`, `events/mod.rs:316`); a
  deterministic flush proves fire-once-per-distinct-activity (mirror
  `drain_records_every_broadcast_event_exactly_once`, `:241`); the `CancellationToken` shuts the
  dispatcher down cleanly (mirror `drain_flushes_buffered_events_on_shutdown_cancellation`, `:343`);
- **in-place session** (`session new` WITHOUT `--branch`, `in_place_target` at `session/mod.rs:977`)
  fires `session-start`/`session-stop`/`agent-state` and fires **NO** `pre/post-create` or
  `pre/post-remove` (RFC edge case 813-819 — documented, not solved beyond this assertion);
- **env-clear + `/dev/null` discard, per session-layer event (concrete, not "reuse B1"):** for
  `session-start` and `agent-state` (the ones with a live cwd/PTY), a hook that dumps `env` to a
  marker file, run with a sentinel `GITHUB_TOKEN`/`ANTHROPIC_API_KEY` and the full `POHUNEK_*`
  handshake set on the daemon process, asserts (a) the secrets are **absent**, (b)
  `POHUNEK_SOCKET_PATH`/`_DAEMON_ID`/`_ENV`/`_PROTOCOL_VERSION` are **absent**, (c) exactly the B.3.1
  allowlist is present. These are distinct `run_hook` spawns from the worktree-bind thread and must
  prove the env discipline independently.

> **Daemon crash is untested by design.** `session-stop` is best-effort-on-clean-teardown — a daemon
> crash bypasses `record_exit` entirely and fires nothing (RFC 376-377, 830-831). Crash-leak cleanup
> is out of scope; this is stated so a reviewer does not flag it as a missing test.

**Checkpoint.** Launching then stopping a session fires `.pohunek/hooks/session-start` then
`session-stop` (the latter with `POHUNEK_STOP_REASON=stopped`); a natural agent exit fires
`session-stop` with `done`/`failed`; an `agent-state` hook fires on real working↔blocked↔idle
transitions only, never on the periodic refresh, and a `sleep 30` agent-state hook does not stall the
event log; an in-place session fires only session-start/stop/agent-state.

---

### Slice B3 — Host-global hook layer

Add the `~/.config/pohunek/hooks/<event>` layer (resolved from the Slice-0 `config_dir`) and compose
host-global-then-in-repo for every event. This is the only slice in Part B that consumes `config_dir`,
so it depends on Slice 0.

**Touch-points & exact changes.**
- `crates/daemon/src/main.rs:77` — already carries `config_dir` (Slice 0 owns that line); B3 adds only
  `hook_timeout`. **Do not** re-add `config_dir`.
- `crates/daemon/src/session/mod.rs:502` — pass `config.config_dir` and `config.hook_timeout` into the
  new `WorktreeManager::new(root, store, hook_timeout, config_dir)` signature (via the `config_dir()`
  accessor decided in Slice 0); the session-layer (`session-start`/`session-stop`) and agent-state
  dispatchers read the same `config_dir` value (a plain `PathBuf`, NOT a `ProjectManager` dependency
  — per the RFC B.2 DI note).
- `crates/daemon/src/worktree/mod.rs` — inside `run_hook`, for a given `<event>` resolve both layers
  and run **host-global first, then in-repo**, when present (compose, not override). This per-event-name
  compose rule is distinct from the in-repo `setup`→`post-create` precedence/fallback (only one of those
  runs) handled in B1. Each composed script is a separate `run_hook`-disciplined spawn (its own
  env-clear, timeout, process-group, `/dev/null`); a failure in one is an independent `Hook` warning and
  does not stop the other.

**Tests** (every B3 DoD bullet; extend `crates/daemon/src/worktree/tests.rs`, seeding a fake
`config_dir/hooks/<event>` via the manager helpers):
- for `post-create`, both layers present → both run, **host-global before in-repo** (assert ordering
  via an append-ordered marker file);
- either layer alone runs (host-only present → host runs; in-repo only → in-repo runs);
- completes the B.1 matrix rows that involve the host layer: `host only` → host; `host + repo setup`
  → host, then repo setup; `host + repo post-create (+setup)` → host, then repo post-create (setup
  ignored) — confirms `setup` is shadowed only by the *in-repo* post-create, never the host-global one;
- **env-clear for host-layer scripts (concrete, distinct spawn):** a host-global hook that dumps `env`
  to a marker file, run with a sentinel `GITHUB_TOKEN`/`ANTHROPIC_API_KEY` and the full `POHUNEK_*`
  handshake set on the daemon, asserts (a) the secrets are **absent**, (b)
  `POHUNEK_SOCKET_PATH`/`_DAEMON_ID`/`_ENV`/`_PROTOCOL_VERSION` are **absent**, (c) exactly the B.3.1
  allowlist is present. Do **not** phrase this as "reuse B1" — the host script is its own `run_hook`
  spawn.

**Checkpoint.** `~/.config/pohunek/hooks/post-create` and `<repo>/.pohunek/hooks/post-create` both run
on a worktree create, host-global first; deleting one leaves the other running; a host-global hook sees
no `GITHUB_TOKEN`; `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt
--check` clean.

---

### Slice C0 — `ShellAdapter` prerequisite + four-site collapse

> **Re-slice note (vs the RFC).** The RFC's Slice C0 bundles "`agent` becomes a free-string name +
> `ShellAdapter`". This plan **splits** it: **C0** is the `ShellAdapter` prerequisite + the four-site
> match collapse (zero behavior change, **no** wire change); the wire enum→string flip,
> free-string resolution, and base-kind filter grouping move to **C1**. Every RFC-C0 DoD bullet
> (`session new --agent claude/codex/shell` unchanged, `agent_profile_not_found`, `invalid_name`,
> roundtrip-to-string, the `session list --filter agent=claude` base-kind-grouping test, `SessionInfo`
> shows the profile name) is therefore a **C1** test — re-mapped explicitly there. C0 lands first within
> the C-track because it is the riskiest single refactor and is independently shippable.

Makes "all three base kinds resolve through one data-driven path" literally true, with **zero behavior
change**.

**Touch-points & signatures.**
- New `crates/daemon/src/agent/shell.rs`: `#[derive(Debug, Default)] pub struct ShellAdapter;`
  `impl AgentAdapter for ShellAdapter`. `id() -> "shell"`; `launch(opts)` builds from
  `ShellCommand::default()` (`session/mod.rs:80`, `$SHELL`→`/bin/sh` fallback) →
  `build_pty_command(&program, args, opts)`; `input_rules() -> InputRules { bracketed_paste: false,
  submit_delay: Duration::ZERO }` (the values currently inline at `session/mod.rs:2136-2139`);
  `manifest() -> crate::detect::generic_shell_manifest()`; `resume(_)` returns the `agent_not_resumable`
  path (Shell has no native resume).
- Relax `AgentAdapter` (`agent/mod.rs:173`): `id() -> &str` (`:175`), `manifest() -> &Manifest` (`:181`).
  Make `launch_command` (`:186`) take `program: &str` (or delete it and have adapters call
  `build_pty_command` (`:199`) directly, which already takes `&str`). `resolve_binary` (`:248`)
  unchanged.
- Collapse the four match sites to dispatch through `&dyn AgentAdapter` (or `ResolvedProfile` in C1):
  - `build_launch_command` (`session/mod.rs:2106`): drop the `AgentKind::Shell =>
    shell_command.to_pty_command(...)` special case; all three go through `adapter.launch(&LaunchOpts{…})`.
  - `input_rules_for_agent` (`:2134`): **preserve the `config.claude_submit_delay` override** (`:2142`)
    — apply it on top of the resolved adapter's `input_rules()` for the claude base; do not silently
    drop it.
  - `DetectorConfig::for_agent` (`detect/mod.rs:53`): already uniform; leave or route through the
    adapter's `manifest()`.
  - `resume_pty_command` (`agent/mod.rs:224`): dispatch via the adapter; Shell still returns typed
    `agent_not_resumable` (`:235`).

**Tests** (DoD: *Shell is an `AgentAdapter` and the four match sites collapse with no behavior change*):
- mirror `crates/daemon/src/agent/mod.rs:358-413` (`codex_launch_resolves_binary_and_preserves_opts`,
  `adapters_return_expected_input_rules`, with the `with_path`/`write_executable` harness `:330-356`):
  add `shell_launch_resolves_binary_and_preserves_opts` and assert `ShellAdapter.input_rules()` ==
  `{bracketed_paste:false, submit_delay:0}`;
- mirror `agent/mod.rs:574-595` (`adapter_manifests_match_agent_specific_rules`):
  `ShellAdapter.manifest()` equals `generic_shell_manifest()`;
- mirror `agent/mod.rs:548` (`resume_pty_command_rejects_shell_agent`): `ShellAdapter.resume(...)` /
  `resume_pty_command` for shell still yields `agent_not_resumable`;
- **regression pin:** `crates/daemon/src/session/mod.rs:3776-3785`
  (`claude_input_rules_use_configured_submit_delay`) must stay green — proves the
  `config.claude_submit_delay` override survives the collapse;
- keep the ~15 `ShellCommand::new("/bin/sh", …)` test fixtures (`session/mod.rs:2995/3013/…`)
  compiling: `ShellAdapter` wraps `ShellCommand`, it does not change the `shell_command` config seam
  (`:128`).

**Checkpoint.** `cargo test -p pohunek-daemon agent` green; `build_launch_command` has no `match agent`
Shell special-case; `pohunek session new --agent shell` behaves identically (manual smoke); clippy/fmt
clean.

---

### Slice C1 — Profile model, loader, free-string `agent` wire + resolution

Introduces the data-driven `AgentProfile`/`ResolvedProfile`, the boot-time owner-only loader, the wire
enum→string flip, the name-resolution chain, and the base-kind filter grouping. Depends on Slice 0
(`config_dir`) and C0.

> **Security framing.** Today the only thing that gates "which binary runs" is the `&'static str`
> program literals baked into each adapter (`resolve_binary`, `agent/mod.rs:248`, only does a PATH
> lookup + executable-bit check on whatever name it is handed). With a free-string `agent`, the
> security story shifts entirely to the A.2.1 charset guard, the owner-only/containment dir gate (C.5,
> in C2), and the no-secrets resume snapshot (C.4, in C2) — **not** to `resolve_binary`, which keeps
> doing exactly what it does today.

**Touch-points & signatures.**
- Wire flip (protocol): apply every `AgentKind`→`String` change in *New & changed types*. Add
  `agent_base: AgentKind` to `SessionInfo` (`session.rs:309`); update both fixtures:
  `crates/protocol/tests/roundtrip.rs:31-54` (`running_shell_session`) and
  `crates/protocol/src/session.rs:423-446` (`session()`).
- New typed errors in `crates/protocol/src/error.rs`: `invalid_name`, `agent_profile_not_found`.
- New `crates/daemon/src/agent/profile.rs`:
  - `AgentProfile` deserialized from `~/.config/pohunek/agents/<name>.toml` (`base`, `program`, `args`,
    `[env]`, `[input_rules]`, `[resume] {mode,ref_kind,resumable}`, optional `manifest = "<name>"`).
  - `ProfileRegistry::load(agents_dir: &Path) -> ProfileRegistry` — the owner-only/containment-gated
    loader (full gate in C2; C1 does the read+parse+resolve).
  - `fn resolve_agent(&self, name: &str) -> Result<ResolvedProfile, ProtocolError>` implementing the
    **exact 4-step chain**: (1) A.2.1 charset guard `^[A-Za-z0-9._-]+$`, no leading `.`/`-`, no
    `/`/`\`/`..`/control → else `invalid_name`; (2) profile file exists → that profile; (3) `name ∈
    {shell,codex,claude}` → bare base kind (`ShellAdapter`/`CodexAdapter`/`ClaudeAdapter` defaults);
    (4) else → **hard `agent_profile_not_found`** (no silent fallback). The A.2.1 guard is a **new
    function** modeled on `validate_git_ref_arg` (`worktree/mod.rs:594`) but stricter — do **not**
    import that one (it uses `invalid_branch` and allows `/`).
  - **Load-time validation:** a profile with `base = "shell"` and `resumable = true` is rejected at
    load (a shell has no native resume).
- Wire `ProfileRegistry` into `SessionRegistryConfig` (`session/mod.rs:126`) and construct it in
  `main.rs:77` from `paths.config_dir.join("agents")` (via the Slice-0 `config_dir`). **Do not** re-add
  `config_dir` to the `main.rs:77` literal — Slice 0 owns it.
- Use `resolve_agent` at the launch sites: `build_launch_command`/`input_rules_for_agent` build from
  the `ResolvedProfile` (program/args/env/input_rules) rather than the bare base kind. **Profile `env`
  control (precise):** strip every `POHUNEK_`-prefixed key from the resolved profile env, then combine
  it with `session_pty_env`'s output (the daemon's `POHUNEK_*` last-write-wins) at the
  `build_launch_command` combine point — full reserved-prefix enforcement + the last-write-wins test in
  C2.
- `crates/cli/src/main.rs:283`/`commands/session.rs:39,53,195,421,746`: `--agent` becomes free-form
  `String`; `agent_label`/`parse_agent_filter` accept arbitrary names; `build_new_request` sends the
  string through.

**Tests** (DoD: *the three built-in names with no profile behave exactly as today; a `<name>.toml`
overrides; an unknown name fails closed* — plus every re-mapped RFC-C0 DoD bullet):
- profile resolution: a `claude-sonnet.toml` (`base="claude"`, `program="claude"`, `args=["--model",…]`)
  resolves to a `ResolvedProfile` whose program/args match the file; **`shell`/`codex`/`claude` with no
  profile file resolve to the bare base-kind defaults** (assert program == `claude`/`codex`/`$SHELL`,
  empty args, base-kind `input_rules`) — *RFC-C0 "behaves unchanged"*;
- **fail-closed:** `resolve_agent("nope")` → `agent_profile_not_found`; `resolve_agent("../etc")`,
  `resolve_agent("a/b")`, `resolve_agent("-x")`, `resolve_agent(".hidden")`, control-char names →
  `invalid_name` (mirror the iterate-bad-inputs pattern at `project/mod.rs:691`, control chars `:710`)
  — *RFC-C0 fail-closed + invalid_name*;
- load rejection: `base="shell"` + `resumable=true` profile is rejected at load;
- adapter launch: mirror `agent/mod.rs:358-413` for a profile-driven launch (resolves the profile
  program over PATH, preserves opts); `missing_agent_binary_returns_typed_error` (`:557`) — a profile
  whose `program` is unresolvable surfaces `agent_binary_missing`;
- `config.claude_submit_delay` still applies to a `base="claude"` profile (extend `:3776`);
- **base-kind filter grouping (re-mapped RFC-C0 DoD 917-918):** create a session whose stored agent
  name is `claude-sonnet` with `agent_base = Claude`; assert `SessionListFilter::Agent("claude")`
  matches it via `agent_base` **and** that `SessionListFilter::Agent("claude-sonnet")` still matches by
  exact name. Mirror the existing list-filter tests (`session.rs:465-498`). Resolving from the snapshot
  (not re-reading the profile at list time) keeps filtering stable after a profile edit/delete (C.4
  consistency);
- **protocol roundtrip blast radius** (mirror `crates/protocol/tests/roundtrip.rs`):
  `agent_kind_json_shape_roundtrips` (`:56`) becomes a free-string shape test; update the ~12 sites
  hardcoding `"agent":"shell"`/`"claude"` (`:91/:120/:151/:178/:208/:270/:299/:410/:434/:816/:1078`,
  integration `:612/:633`); `host_capabilities_json_shape_roundtrips` (`:1078`) asserts
  `supported_agents: ["shell","codex","claude"]` as strings; new `new_remote_error_codes_are_distinct`
  cases (`:1221`) for `invalid_name`/`agent_profile_not_found`.
  **`session_list_filter_bad_value_is_a_deserialization_error` (`:259`)** no longer rejects a bad
  *agent* value (free string) — relocate its agent-value assertion to a daemon-side resolve-time
  `invalid_name` test; the `state` rejection stays — *RFC-C0 "roundtrip tests updated to string shape"*;
- **`SessionInfo` shows the profile name** (re-mapped RFC-C0 DoD): a session launched with
  `--agent claude-sonnet` reports `SessionInfo.agent == "claude-sonnet"` and `agent_base == Claude`.

**Checkpoint.** `pohunek host inspect` lists `shell/codex/claude` plus any loaded profile names;
`pohunek session new --agent claude-sonnet` launches with the profile's program/args/env;
`pohunek session new --agent bogus` fails with `agent_profile_not_found`; `pohunek session list
--filter agent=claude` matches a `claude-sonnet` session; full `cargo test` green.

---

### Slice C2 — Detection/resume inheritance, C.4 snapshot, C.5 security & host.inspect

Closes the inheritance, the no-secrets resume snapshot, and the full security gate. Depends on C1.

**Detection inheritance (C.3).** `DetectorConfig::for_profile(base, override_manifest)` (new,
`detect/mod.rs:38` struct, `:40` seam field). When the profile sets `manifest = "<name>"`, resolve
`~/.config/pohunek/agents/manifests/<name>.toml` under the **same A.2.1 charset + canonicalize-and-contain
guard**, parse via `Manifest::parse_str` (`detect/manifest.rs:22`, caps already enforced), and on
`Err(ManifestError)` **disable that one profile + warn** — never `.expect`-panic the daemon. Decide and
document: an **empty-rule manifest** (parses OK, detection-disabled) is **accepted**, not a load error.

**Resume inheritance (C.3).** `resume_pty_command` builds argv from `ResumeMode` (`Flag → ["--resume",
<ref>]`, `Subcommand → ["resume", <ref>]`) and `ref_kind` picks `SessionRef::id` (`agent/mod.rs:81`)
vs `SessionRef::path` (`:116`). A non-resumable profile yields `agent_not_resumable`. **Document the
guard asymmetry:** `id` carries the leading-dash argv-injection guard; `path` carries the
must-be-absolute guard — a `ref_kind="path"` profile inherits the *absolute-path* guard, not the dash
guard.

**`report_native_id` path-kind storage (C.3 — the load-bearing fix).** Flipping only the ctor is dead
code: today the validated value is stored unconditionally into `entry.info.native_session_id`
(`session/mod.rs:1326`), `persist_resume_binding` hardcodes `native_session_path: None` (`:1371`), and
`resume_binding`'s apply match `(Some(id), _) => SessionRef::id(id)?` (`:1450`) always wins — so a
`ref_kind="path"` profile would silently resume via `id` (dash guard), the opposite of the documented
behavior. The fix is **three coordinated edits**:
1. In `report_native_id` (`:1288`), look up the session entry's frozen `ref_kind` from the launch-time
   snapshot (the entry is already fetched via `sessions.get_mut(&params.session_id)` at `:1308`) and
   **pick the destination FIELD by kind, not just the ctor**: for `path`, build `SessionRef::path` and
   store into `entry.info.native_session_path`, leaving `native_session_id` `None`; for `id`, store
   into `native_session_id` as today. **Ignore `params.agent` for `ref_kind`** — the SessionStart hook
   bakes the base-kind literal (`agent = "claude"`,
   `crates/daemon/src/integration/assets/claude/pohunek-agent-state.sh:41`) and the handshake env carries
   no profile identity, so the wire `agent` is informational only.
2. In `persist_resume_binding` (`:1349`, literal `:1364-1376`), copy `entry.info.native_session_path`
   through **instead of the hardcoded `None`** at `:1371`.
3. `resume_binding`'s existing match (`:1449-1451`) then selects `::path` correctly when
   `native_session_id` is `None` and `native_session_path` is `Some`.

**C.4 structural snapshot (capture sites spelled out).** `persist_resume_binding` rebuilds the
`ResumeBinding` literal fresh from `entry.info` on **every** call (creation, `report_native_id` at
`:1330`, the hot resize path at `:1795`). "Copy verbatim" is only achievable if the structural fields
**exist on the entry** and are written into the literal at every persist. So:
1. Add the structural fields (`program`, `args`, `input_rules`, `resume_mode`, `ref_kind`, `resumable`,
   `agent_base` — **NOT `env`**) to the in-memory session entry struct (behind `entry.info` or a
   sibling), **set once at `register_pty_session`/`create`** from the `ResolvedProfile`.
2. Add the same fields to the `ResumeBinding` literal in `persist_resume_binding` (`:1364-1376`),
   **reading from the entry** — never re-resolving from disk (which on the resize path would overwrite
   the frozen values and re-open the window this closes).
3. Thread them back through `resume_binding` (`:1448`) into the `PtySessionSpec` (`:1485`-region):
   structural fields from the binding; **`env` re-resolved from the profile by name** at
   `env_extra`/`session_pty_env` (`:1465`); a deleted profile resumes from the frozen structural
   snapshot with **no profile env + a warning**; a genuinely missing snapshot yields a typed error
   (mirror the existing `not_resumable` arm `:1452-1460`), never a panic.
- **`env` is never a field of `ResumeBinding`** — make this an explicit, tested constraint; extend the
  store doc header's non-secret enumeration (`store/mod.rs:26-28`) to list the seven added structural
  fields as the only additions.

**C.5 security gate (in `ProfileRegistry::load`).** `#[cfg(unix)]`: verify `agents/` **and**
`agents/manifests/` are owned by the daemon user and not group/world-writable (mode `& 0o022 == 0` +
owner uid check) — else skip the **whole** dir + warn (fail-closed, do not load any profile). For every
resolved `agents/<name>.toml` **and** manifest file, `std::fs::canonicalize` and assert lexical
containment within the canonicalized `agents/` tree (`starts_with`) — owner-checking the file alone is
insufficient (a symlink execs the link target). Enforce the **whole `POHUNEK_` prefix** as reserved:
when merging a profile `[env]`, drop any key starting with `POHUNEK_` (covers
`POHUNEK_SESSION_ID`/`POHUNEK_DAEMON_ID`/`POHUNEK_SOCKET_PATH`/`POHUNEK_ENV`/`POHUNEK_PROTOCOL_VERSION`
and any future one). **Remote = fail-closed:** a profile *name* resolves against the target daemon's
set; a *definition* is never accepted over the wire.

**`host.inspect` enumeration.** `host_capabilities` (`capabilities.rs:20`) enumerates loaded profile
names + base kinds and probes each `program`; unify the two `which` semantics (`which_on_path` `:73`
`.is_file()` vs `is_executable_file` `agent/mod.rs:260` `0o111`) so "available" agrees with
`resolve_binary`.

**Hook opt-in inheritance.** `install` (`crates/daemon/src/integration/mod.rs:73`) maps a profile to
its base `AgentKind`, then routes to `install_claude`/`install_codex`; `base="shell"` stays
non-installable. No new asset — the verbatim claude/codex asset's hardcoded `agent="claude"`/`"codex"`
is the base-kind name the daemon ignores for `ref_kind` anyway.

**Tests** (every C.3/C.4/C.5 DoD bullet):
- **detection override:** mirror `crates/daemon/src/detect/mod.rs:963`
  (`detector_config_for_agent_loads_agent_manifest`) for `for_profile` with an override manifest (use
  the inline-`parse_str` helper at `:463`); a malformed host manifest path returns `Err` and disables
  only that profile (assert the daemon does not panic and other profiles still load) — drive the typed
  variants from `manifest.rs:1220` (`invalid_state`)/`:1262` (`invalid_regex`); **an empty-rule manifest
  loads successfully with detection disabled** (not a load error), confirming the documented decision.
  No new cap tests (caps proven at `manifest.rs:1073/1163`);
- **resume inheritance:** mirror `agent/mod.rs:416-426` (`resume_builders_match_native_agent_argv`) — a
  `mode="flag"` profile produces `["--resume", <id>]`, `mode="subcommand"` produces `["resume", <id>]`;
  mirror `:478-520` (`session_ref_path_*`, `resume_argv_carries_path_kind_value`) for `ref_kind="path"`;
  mirror `:459` (`session_ref_id_rejects_leading_dash_…`) for `ref_kind="id"`; a non-resumable profile →
  `agent_not_resumable` (`:548`);
- **`report_native_id` ref_kind from profile (the blocker fix):** extend
  `crates/daemon/src/session/mod.rs:3375` (`report_native_id_records_binding_and_updates_info`) and
  `:3731` (`report_native_id_ignores_unknown_invalid_and_terminal`): a **path-based** profile makes
  `::path` the chosen ctor, stores into `native_session_path` (leaving `native_session_id` `None`), the
  persist literal copies the path through, and `resume_binding` resumes via `::path` — while the wire
  `agent` string is ignored;
- **C.4 snapshot round-trip:** mirror `crates/daemon/src/store/mod.rs:903`
  (`worktree_project_id_round_trips_and_a_legacy_line_loads`) per new field (set value round-trips; a
  legacy line without it loads with the serde default); extend the `resume(session_id, native)`
  fixture; `a_corrupt_line_is_skipped…` (`:1006`) and `store_file_is_owner_private` (`:722`, mode
  `0o600`) stay green. Mirror `session/mod.rs:3457`/`:3621` (resize/recapture) to prove the frozen
  snapshot survives `report_native_id` + resize **verbatim** and restores on resume (this is the RFC's
  *"after editing program/args/input_rules post-creation and forcing a resize re-persist, a
  restart-resume still uses the original structural values"* DoD); `:3416`/`:3686` (binding dropped on
  stop) unaffected;
- **no-secrets store invariant (substring scan, not just "no env field"):** a resolved profile with
  `[env]` set produces a serialized `ResumeBinding` JSON line that contains **none** of the profile's
  `[env]` keys **or values** anywhere in the line (substring scan of the env values), and re-resolves
  `env` at resume (a deleted profile → no profile env + warning);
- **C.5 gate (`#[cfg(unix)]`):** a group/world-writable `agents/` dir is skipped + warned (no profiles
  loaded); **a group/world-writable `agents/manifests/` dir** is likewise skipped + warned; a symlinked
  `agents/<name>.toml` escaping the tree is rejected by containment; **a profile naming a manifest that
  resolves (via canonicalize) outside the `agents/` tree** is rejected by containment;
- **POHUNEK_ reserved + daemon-wins (joint test):** a profile `[env]` with `POHUNEK_FOO`,
  `POHUNEK_ENV=0`, and `POHUNEK_PROTOCOL_VERSION=99` produces a final PTY env (assert against the actual
  `session_pty_env`-combined output, not just that the key was dropped from the profile map) where
  `POHUNEK_FOO` is **absent** and `POHUNEK_ENV`/`POHUNEK_PROTOCOL_VERSION` carry the **daemon's** values
  — never the profile's;
- **host.inspect:** extend `crates/daemon/src/capabilities.rs:88`
  (`snapshot_reports_…three_supported_agents`) — supported agents now include loaded profile names;
  preserve the `agent_runtime_availability_matches_resolved_path` invariant (`:116`, `available ==
  path.is_some()`) for probed profile programs;
- **hook opt-in inheritance:** mirror `crates/daemon/src/integration/mod.rs:820`
  (`install_claude_into_fresh_dir_writes_executable_hook_and_session_start`) — a `base="claude"`
  profile routes to `install_claude`; keep `assets_fire_our_method_with_our_env_and_exit_zero_on_missing_env`
  (`:779`) green.

**Checkpoint.** A host-authored `claude-sonnet.toml` (id resume) and a `path`-resume profile both
launch, detect, and resume correctly across a simulated daemon restart; editing the profile's `[env]`
after launch and restarting picks up the new env while structural relaunch is unchanged; deleting the
profile still relaunches from the frozen snapshot with a warning; a world-writable `agents/` (and
`agents/manifests/`) is skipped; a profile `[env]` cannot override any `POHUNEK_*` key; `pohunek host
inspect` lists all resolvable agents with availability; `cargo test && cargo clippy --all-targets &&
cargo fmt --check` clean.

## Build order & checkpoints

The RFC's "three parallel tracks that don't share files" is **not literally true** — at least three
files are co-edited across tracks, two on the **same** struct lines:

- `crates/daemon/src/session/mod.rs` `SessionRegistryConfig` (struct `:126`, `Default` `:382`): Slice 0
  adds `config_dir`; Part B renames `setup_script_timeout` → `hook_timeout`; Part C adds the profile
  registry. These are guaranteed textual conflicts.
- `crates/daemon/src/main.rs:77` (one struct literal): Slice 0 adds `config_dir`, B3 adds
  `hook_timeout`, C1 adds the agents-dir line; B3/C1 both reference `paths.config_dir`, which only
  exists after Slice 0.
- `crates/protocol/src/session.rs` and `crates/protocol/tests/roundtrip.rs`: Part B adds
  `SessionWarningKind::Hook` + extends `session_warning_json_shape_roundtrips`; Part C does the
  enum→string flip + rewrites the ~12 hardcoded agent sites. `roundtrip.rs` is a real shared-edit
  surface.
- `crates/daemon/src/session/mod.rs` `record_exit`/`persist_resume_binding`/`resume_binding`: Part B
  fires `session-stop` in the `record_exit` post-lock tail (`:2027-2028`); Part C edits the
  persist/resume literals (`:1364-1376`, `:1448`). Same ~80-line windows.

**Tracks may be DEVELOPED in parallel but MUST be MERGED serially, with explicit seam ownership.** The
recommended real merge order:

```
Slice 0  config_dir            ── OWNS config_dir on SessionRegistryConfig + the main.rs:77 line + config_dir()
   │                              accessor. B3 and C1 must NOT re-add config_dir.
   ▼
A1 → A2 → A3  (Part A)         ── project.* methods, resolver, launcher. Touches project/config.rs (new),
   │                              api/handler.rs, protocol/project.rs — no overlap with B/C.
   ▼
C0 → C1 → C2  (Part C)         ── lands BEFORE B's WorktreeRequest.agent work, so params.agent is already a
   │                              String and B1 writes NO AgentKind→label shim. C owns the agent enum→string
   │                              flip in session.rs and the protocol roundtrip rewrite.
   ▼
B1 → B2 → B3  (Part B)         ── run_hook + events. Rebased on C: B sources a String params.agent directly.
                                  B adds SessionWarningKind::Hook (reconcile roundtrip.rs against C's rewrite)
                                  and the hook_timeout rename on SessionRegistryConfig + main.rs:77.
                                  B2/C2 co-edit record_exit's post-lock tail — B (merged last) rebases and
                                  re-verifies the resume-binding-drop vs hook-fire ordering.
```

Within the C-track, **C0 is first and the riskiest single step** (it touches every `AgentKind` adapter
site) and is independently shippable with zero behavior change. C0 is "alone" *within Part C*, but it
has a cross-track consumer: Part B's `WorktreeRequest.agent` depends on C0 having flipped `params.agent`
to a `String` — hence C before B.

Each milestone ends green (`cargo test --workspace && cargo clippy --all-targets -- -D warnings &&
cargo fmt --check`). Slice 0 adds no user-visible behavior; A1–A2 and C0–C1 add no user-facing change
until their last slice; A3, B1–B3, and C2 are the observable changes.

## No backward compatibility (experimental project)

Same stance as the Projects work ([`projects-plan.md`](projects-plan.md)) and
[`NEXT.md`](../../NEXT.md): **backward compatibility is an explicit non-goal.** This plan ships
genuinely breaking changes; none carry a compat shim or migration.

- **CLI and daemon are the same build.** We never handle an old CLI talking to a new daemon (or vice
  versa) — no `method_not_found` "please upgrade" translation, no version-negotiation shims. The
  operator upgrades all hosts' daemons together (already "a session-killing event by design").
- **Wire shapes change freely.** Concretely: `agent` flips from the `AgentKind` unit enum to a **free
  string** across `SessionNewParams`, `SessionInfo` (+ the new `agent_base`), `SessionListFilter`,
  `SessionReportNativeIdParams`, and `HostCapabilities`; the `project.actions`/`project.action`/
  `project.prompt` methods and the `SessionWarningKind::Hook` unit variant are added **without** a
  `PROTOCOL_VERSION` bump (additive policy, `version.rs`). The pre-1.0 roundtrip tests
  (`crates/protocol/tests/roundtrip.rs`) are **rewritten** to the new shapes, not kept dual-form.
- **On-disk store may be wiped on upgrade.** `WorktreeBinding` gains `agent` (a name); `ResumeBinding`
  gains the agent name + the C.4 **structural** snapshot (program/args/input_rules/resume_mode/ref_kind/
  resumable/`agent_base`) — and **never** the profile `env`. We write **no migration**; a pre-existing
  store that lacks these fields is discarded, not upgraded. Serde defaults are used only where they keep
  the code simple, not as a compat guarantee.
- **Config formats change freely.** Daemon-side definition formats are **TOML**
  (`.pohunek/{templates,actions}.toml` + host equivalents, read by the Rust `toml` crate); agent
  profiles are TOML; the `${var}` prompt contract may change. The **client** `launcher.conf` **stays
  flat `key=value`** (parser unchanged) but **shrinks**: the per-launch keys now resolved daemon-side
  (`agent`/`project`/prompt selection) move out, leaving only host-only keys (`host`, `terminal`,
  `linear_cli`, `list_timeout_seconds`, …). The operator re-runs `pohunek setup`; removed keys are
  **not** kept valid.

## Risks & mitigations

- **A checked-out repo influencing what the daemon execs (no trust gate).** *Mitigation:* the A.5
  safe-subset — `RawTemplate`/`RawAction` carry only `agent` (a **name**), `base_branch`, `branch` rule,
  and a prompt *name* (fed as `--input`, never executed); `program`/`argv`/`args`/`flags`/`env` are
  rejected structurally by `deny_unknown_fields` (A2 test). `program`/`argv`/`env` live only in a base
  kind or a host profile (Part C).
- **Daemon reads repo-named files and returns their bytes (path traversal / symlink escape).** New with
  the daemon-resolves pivot. *Mitigation:* the A.2.1 charset guard (`invalid_name`) + real
  canonicalize-and-contain within `<repo_root>/.pohunek/` (resp. `<config_dir>/`, the host base) — **not**
  the best-effort `canonical_or_original` — + a read surface limited to `prompts/*.tmpl` and the two
  TOMLs. Applies identically to the in-repo `prompt=` field and the wire/CLI `<name>`, and to **both**
  layers (A1 host-layer symlink test).
- **Free-string `agent` on the wire + operator-defined binaries (Part C).** *Mitigation:* the wire
  string is a **name** (A.2.1 guard, fail-closed `agent_profile_not_found`) that resolves only to a
  compiled base kind or a host profile — it can never *be* a program. The only new exec surface is
  `~/.config/pohunek/agents/`, which is **owner-only + containment-gated** (skipped + warned otherwise)
  and never accepted over the wire. The wire caller is already trusted (single operator behind the
  NetBird boundary).
- **Arbitrary code execution via hooks (no trust gate).** Same posture as the existing `.pohunek/setup`,
  broadened to seven events. *Mitigation:* `.env_clear()` to an allowlist (no inherited
  `GITHUB_TOKEN`/`ANTHROPIC_API_KEY`/`POHUNEK_SOCKET_PATH` — the biggest exfiltration vector), output
  discarded, timeout + process-group. The env-clear is the one hardening this work does **not** defer;
  the residual risk (arbitrary code as the daemon user within the timeout) and a trust gate are out of
  scope per the decision.
- **`read_all`/persist hot paths.** The C.4 snapshot rides through `persist_resume_binding`, which runs
  on the resize path; copying verbatim (never re-resolving) is correctness, not back-compat. Fully
  covered by the resize/recapture snapshot tests.
- **A hook wedging `session.new`.** Hooks reuse the *bounded* setup-script discipline, unlike
  `WorktreeManager`'s own unbounded git executor (`worktree/mod.rs:957`), so a hung hook cannot wedge
  create. `agent-state` runs on a separate broadcast-subscriber task with last-fired tracking + debounce,
  never blocking the audit-log drain.
- **Extra daemon round-trip per launch.** `project.action` adds one request before `session new`. Cheap
  (the daemon is already up) and the cost of one source of truth.

## Out of scope

- Mutating/vetoing hooks (return values that change git args, paths, env, or abort). v1 is strictly
  react-only.
- Daemon-side **rendering** of prompts / a `session new --action` that fetches provider data — blocked on
  providers being daemon-reachable (Phase 4 Slice E). v1 keeps fetch+render caller-side.
- Arbitrary `program`/`argv`/`env` from a **repo/template or the wire** (the unsafe superset of A.5) —
  these live only in a host agent profile (Part C).
- Shipping an agent profile **definition** over the wire (re-opens A.5); profiles are host-authored only.
  Profile **inheritance graphs** / per-profile permission policies are also out — a profile extends
  exactly one base kind.
- Per-project **detection manifest** overrides from a repo (`detect/manifests/*.toml`); a *host* profile
  may override its manifest (C.3), a repo may not.
- A trust/approval gate or hash-pinning for repo-supplied code; crash-leak cleanup (a daemon crash fires
  no `session-stop`).
- New provider adapters; `provider` stays `linear_issue`/`github_pr`/`none`.
- Storing definitions/profiles in the metadata store (only the non-secret resume snapshot rides there);
  a `worktree.*` RPC method; `<event>.d/` multi-script hook directories (single-file form first).
