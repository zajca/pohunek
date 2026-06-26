# Design: Per-Project Actions/Prompts, Per-Host Agent Profiles & Worktree Hooks

Status: **accepted and implemented** (verified on 2026-06-26). The full feature —
all three parts below — is implemented and merged into `main`; see the companion
[`*-plan.md`](per-project-actions-and-worktree-hooks-plan.md) for the
slice-by-slice landing record.

This RFC proposes three related features that share one config model:

- **Part A — Per-project templates, actions & prompts** (daemon-resolved): make the
  launcher's "process this issue / process this PR" flow configurable **per
  project**, with the **daemon** as the single source of truth so both the rofi/sway
  launcher (today) and the browser control center (Phase 4) read identical
  definitions. An **action** is a named launchable operation; a **template** is the
  recipe it runs; a **prompt** is the agent instruction the template feeds. A
  per-project definition is found in the project's repo, else a host default, else —
  for prompts — a hard error.
- **Part B — Worktree lifecycle hooks** (daemon-side): generalize the single
  `.pohunek/setup` script into named, react-only hooks that fire across the
  worktree/session lifecycle (create, remove, session start/stop, agent state
  change).
- **Part C — Per-host agent profiles** (daemon-side): turn the hardcoded
  `shell`/`codex`/`claude` adapters into **host-authored named agent definitions**
  (`~/.config/pohunek/agents/<name>.toml`) that extend a built-in base kind with a
  program/args/env/input-rules/resume/manifest override. This is the **trusted,
  host-config-only home** that Part A's A.5 already reserved for
  `program`/`argv`/`env`: a template (even an in-repo one) may **name** a profile but
  may never **define** one.

All three are **daemon-resolved** and **filesystem-discovered**. Parts A and B are
**project-scoped**, layered in-repo `.pohunek/` over host `~/.config/pohunek/`; Part
C is **host-scoped** (a profile is the operator's own host config, never read from a
repo). They differ in kind and failure mode: Part A is declarative config consumed by
a caller (missing prompt = error); Part B is executed scripts (missing hook = skip);
Part C is the trusted definition of *what agent a name resolves to* (missing profile
name = error). All keep the existing **no-trust-gate** posture, bounded by the
constraints in [Security & trust](#security--trust).

This is an **experimental project with no backward compatibility** — the changes here
(notably the `agent` enum → free-string wire change) break wire and on-disk shapes
freely, with no migration or version shim. See
[No backward compatibility](#no-backward-compatibility-experimental-project).

---

## Objective

Today both "what happens when I launch work on an issue/PR" and "what runs when a
worktree is created" are **one-size-fits-all**, and the launch recipe lives only on
the client:

- The launcher reads a single host-wide [`launcher.conf`](../../scripts/lib.sh)
  (`crates/cli/src/commands/setup.rs:73` `LAUNCHER_CONF`): one `agent`, one
  `project`, one `issue.tmpl`/`pr.tmpl`. Every issue, in every repo, is processed by
  the same agent with the same prompt — and the rules live in client-side shell, so
  a second client (the Phase 4 browser) would have to re-implement them and could
  drift.
- A worktree's only post-create extension point is a single committed
  `.pohunek/setup` script (`crates/daemon/src/worktree/mod.rs:51`,`:269`,`:827`),
  with no pre-create, removal, or session-lifecycle counterparts.

We want per-project control of the launch recipe and prompts, resolved **by the
daemon** (so it is the one source of truth for every client and works for remote
projects), plus a general but constrained hook surface around the worktree/session
lifecycle.

## User value

1. **Different projects, different agents/prompts.** Repo A's issues run `claude`
   with a strict TDD prompt; repo B's run `codex` with a lightweight prompt; a docs
   repo's PRs run a review-only prompt. The repo declares it in `.pohunek/`; nothing
   to edit per machine.
2. **One source of truth, two clients.** The rofi launcher (now) and the browser
   control center (Phase 4, `docs/phases/04-browser-control-center.md:256`) ask the
   **daemon** for the same actions/templates/prompts, so they never drift.
3. **Per-repo prompt override that fails loudly.** A project either has the prompt
   it asks for (in-repo) or inherits a host default; if **neither** exists it is a
   hard error, never a silent wrong-prompt — the same fail-fast posture the project
   applies to config everywhere.
4. **Project-specific lifecycle automation.** A repo declares "after the worktree is
   created, start the dev DB; before it's removed, stop it" in `.pohunek/hooks/`,
   travelling with the repo.

## Prior art (what exists today)

- **The launcher action flow** (`scripts/`): `pohunek-rofi-issue` lists my Linear
  issues → `pohunek-launch-issue <id>` reads `launcher.conf`, renders
  `~/.config/pohunek/prompts/issue.tmpl` against the provider JSON
  (`pohunek_render_provider_prompt`, `scripts/lib.sh:106`), derives the branch, and
  runs `pohunek session new --agent A --project P --branch B --input <prompt>`
  (`pohunek_run_session_new`, `scripts/lib.sh:175`). **All client-side shell; the
  daemon knows nothing about "process an issue".** Part A moves the *definitions*
  (which agent, which prompt) into the daemon, leaving provider fetch + rendering on
  the client (see [A.4](#a4-rendering-split-provider-data-stays-caller-side)).
- **The setup script** (`crates/daemon/src/worktree/mod.rs:827` `run_setup_script`):
  runs `<worktree>/.pohunek/setup` via `sh` in its own process group, bounded by
  `setup_script_timeout` (default 300 s, `session/mod.rs:59`), **stdout/stderr
  discarded to `/dev/null`**, failure → non-fatal `SessionWarningKind::SetupScript`.
  The prototype Part B generalizes.
- **The existing per-project override** (`ProjectRecord.default_base_branch`,
  `store/mod.rs:148`), resolved `request-param || project-field || fallback` at
  `session/mod.rs:952`. Part A's resolution follows the same most-specific-wins
  shape and lives next to it in `ProjectManager`.
- **The project resolution + repo path** already in the daemon: `ProjectManager`
  resolves a `<id|label>` to a `ProjectRecord` carrying `repo_root` /
  `git_common_dir` (`project/mod.rs:71`), which is what lets the daemon read the
  project's in-repo `.pohunek/` directly.
- **The `project.*` protocol family** (`protocol/src/project.rs`, dispatched at
  `api/handler.rs`) — the additive pattern Part A's new methods follow.
- **The agent integration "hook"** (`integration/mod.rs`): the Claude/Codex
  `SessionStart` hook capturing the native session id for resume. **Unrelated to the
  worktree lifecycle** — the name collision is why this RFC says *lifecycle hooks*.

## Terminology

| Term | In this RFC | Not to be confused with |
|---|---|---|
| **action** | a named launchable operation (`process-issue`, `process-pr`, custom) the daemon resolves for a project | the CLI subcommand enums (`SessionAction`, `ProjectAction`) |
| **template** | the recipe an action runs: `{agent, base_branch, prompt, branch rule}` | the detection `Manifest` (`detect/manifests/*.toml`) |
| **prompt** | a named agent-instruction template (`${var}` placeholders); resolved fail-closed | a prompt is **one field** of a template |
| **agent profile** (Part C) | a host-authored named agent definition (`~/.config/pohunek/agents/<name>.toml`) extending a base kind | the **base kind** (`shell`/`codex`/`claude`) it inherits from |
| **base kind** (Part C) | one of the three compiled-in agents a profile extends (supplies default manifest/resume/input-rules) | the wire `agent` field, which is now a free-string **name** resolving to a profile or a bare base kind |
| **lifecycle hook** | a react-only script fired by the daemon at a worktree/session event | the agent-state `SessionStart` integration hook |

---

# Part A — Per-project templates, actions & prompts (daemon-resolved)

## A.1 Data model

An **action** binds a trigger to a **template**; a template names a **prompt**.
Declarative TOML, stored in the project's repo and/or host config, **read and
resolved by the daemon**:

```toml
# A template: a reusable launch recipe.
[template.tdd-claude]
agent = "claude"            # an agent NAME — a base kind (shell|codex|claude) or a
                            # host profile (Part C); a NAME only, never a definition (A.5)
base_branch = "main"        # optional; else project default / repo HEAD
prompt = "issue"            # prompt NAME, resolved fail-closed (A.2)

# An action: a launchable operation that uses a template.
[action.process-issue]
provider = "linear_issue"   # linear_issue | github_pr | none
template = "tdd-claude"
# branch rule comes from the provider (Linear branchName / PR headRefName);
# a `none`-provider action specifies branch explicitly or omits it (in-place).
```

A template's fields are exactly the inputs `pohunek session new` already accepts
(`agent`, `base_branch`, `branch`) plus a prompt name. **A template may not inject
an arbitrary program, argv, or environment** into the agent process — see
[A.5](#a5-what-an-in-repo-definition-may-set-security). `provider` selects which
fetch+render path the *caller* uses (today `linear_issue` / `github_pr`).

**Schema (normative for implementation):**

- **`[template.<name>]`** — `agent: string` (required; an agent name, A.5), `prompt:
  string` (required; a prompt name, fail-closed), `base_branch: string` (optional;
  else project default → repo HEAD). Unknown keys are a typed `invalid_template`
  error (no silent ignore — fail-fast).
- **`[action.<name>]`** — `template: string` (required; must resolve), `provider:
  enum` (required; `linear_issue | github_pr | none`), `branch: string` (optional;
  only meaningful for `provider = none` — for `linear_issue`/`github_pr` the branch
  comes from the provider's `branchName`/`headRefName` and an explicit `branch` is an
  `invalid_action` error). Unknown keys → `invalid_action`.
- `<name>` for templates, actions, and prompts must pass the A.2.1 single-segment
  charset guard.

## A.2 Resolution (daemon-side, project-scoped, layered)

The daemon already resolves the target project for a request (`ProjectManager`,
`project/mod.rs:71`) and holds its `repo_root`. Resolution layers, most-specific
first:

1. **in-repo** `<repo_root>/.pohunek/{templates,actions}.toml`,
   `<repo_root>/.pohunek/prompts/<name>.tmpl` — travels with the repo. **The daemon
   reads this directly** because it runs on the host where the repo lives — so it
   works for **remote** projects too (no client filesystem access needed; the
   former client-side remote limitation is gone).
2. **host default** `~/.config/pohunek/{templates,actions}.toml`,
   `~/.config/pohunek/prompts/<name>.tmpl` on the daemon's host.
3. **built-in** seed values shipped by `pohunek setup` (host layer is just where
   they land).

Resolution rules:

- **A named template/action** is resolved **per name, most-specific layer wins whole**
  (in-repo `[template.X]` shadows the host `[template.X]` entirely — table entries are
  not field-merged across layers, to keep "what will run" obvious from one file). The
  *set* of available actions/templates is the **union** of names across layers (so a
  host default action stays available unless the repo redefines that name). A
  template's missing `base_branch` still falls through to the project default → repo
  HEAD (the one scalar fallthrough, matching `default_base_branch`,
  `session/mod.rs:952`).
- **A prompt `.tmpl`**: **first-existing wins** (whole files, not merged) — in-repo
  `prompts/<name>.tmpl`, else host `prompts/<name>.tmpl`, else **a hard
  `prompt_not_found` error**. No silent fallback to a built-in the operator did not
  ask for (fail-fast).
- **Typed errors** (all surfaced to the caller, no silent fallback):
  - **not-found:** `prompt_not_found`, `action_not_found`, `template_not_found` (an
    action names a template that resolves nowhere).
  - **bad name** (charset/containment guard, A.2.1): a single neutral
    **`invalid_name`** for *any* name kind (prompt/action/template/agent/manifest) — the
    guard is shared, so one code keeps it unambiguously testable; the message says which.
  - **bad schema** (unknown/invalid fields): the domain-specific `invalid_action` /
    `invalid_template`.
  These join the existing `project.*` error vocabulary.

### A.2.1 Name & path safety (the daemon reads repo-named files)

The daemon joins a **name** into a host path and returns the file's **content** over
the wire. The name comes from an untrusted source — both the wire/CLI argument and
the in-repo `templates.toml` `prompt = "<name>"` field — so two guards are
**normative**, enforced daemon-side **before any filesystem read**:

1. **Single-segment charset guard.** A prompt/action/template name must match
   `^[A-Za-z0-9._-]+$`, be non-empty, and not begin with `.` or `-`; any `/`, `\`,
   `..`, or control char is rejected with a typed **`invalid_name`** (one neutral code
   for every name kind — prompt/action/template/agent/manifest — since the guard is
   shared; mirroring `validate_git_ref_arg`, `worktree/mod.rs:594`, and the `p-…` id
   discipline). This blocks `prompt = "../../../../etc/passwd"` → arbitrary-file read.
   The guard is identical for the wire/CLI `<name>` and the in-repo `prompt=` value.
2. **Symlink/containment check — for EVERY read, not just prompts.** Git checks out
   symlinks, so a charset-clean `prompts/innocent.tmpl` *or* `templates.toml` *or*
   `actions.toml` can be a symlink to `/etc/shadow` or `~/.ssh/id_rsa`. **Every file
   the daemon reads** — both `.pohunek/{templates,actions}.toml` and
   `.pohunek/prompts/*.tmpl` (and their host-layer counterparts under
   `~/.config/pohunek/`) — is canonicalized and must be **lexically contained within**
   `<repo_root>/.pohunek/` (resp. the host config dir); a symlink that escapes is
   rejected with a typed error. Without this, `project.prompt`/`.action`/`.actions`
   would return arbitrary host-file content under the no-trust-gate posture — **no
   session even needs to start**, a clone + `project actions` suffices.

The read surface is restricted to exactly `.pohunek/{templates,actions}.toml` and
`.pohunek/prompts/*.tmpl` (and the host-layer equivalents); nothing else under the
repo or the config dir is ever read or returned, and the containment guard applies to
all of them uniformly.

## A.3 Protocol & CLI

New, additive `project.*` methods (open string consts, no `PROTOCOL_VERSION` bump —
`protocol/src/version.rs`), each resolving against the project's host so they honor
`--host` like the rest of `project.*`:

- `project.actions` → list the actions resolved for a project (so rofi and the
  browser render the same menu).
- `project.action` → resolve one action to its full recipe:
  `{provider, agent, base_branch, branch-rule, prompt-name, **resolved prompt
  template content**}` — everything the caller needs *except* provider data.
- `project.prompt` → resolve a prompt by name to its template content, **fail-closed**
  (`prompt_not_found`). The primitive behind `project.action` and the CLI.

CLI (mirrors `project show`/`rename`, `cli/src/commands/project.rs`): a new
`ProjectAction` enum variant per command, alongside the existing `List`/`Add`/`Show`/
`Rename`/`Rm` (`crates/cli/src/main.rs:129`):

```
pohunek [--host H] project prompt  <project> <name>     # resolve ONE prompt by name (which layer wins); or error
pohunek [--host H] project actions <project>            # list resolvable actions (+ the template each uses)
pohunek [--host H] project action  <project> <name>     # resolve ONE action to its recipe + prompt content
```

`<name>` is **required** for `project prompt` — `project.prompt` resolves a prompt *by
name* and there is no "default prompt" concept (a missing name is a usage error, a
non-resolving name is `prompt_not_found`). Discovery of available prompt names is not a
v1 command (a future `project prompts` plural could list them; templates already name
the prompts they use, surfaced by `project actions`).

`project action` (singular) is the command the launcher calls (A.4 / Slice A3): it
returns the resolved recipe `{provider, agent, base_branch, branch-rule,
prompt-content}`. `project actions` (plural) lists them. There is no separate
`project template` command in v1 — a template is always reached through its action.

**Resolution lives in a dedicated `ProjectConfigResolver`, not in `ProjectManager`.**
`ProjectManager` is store-glue today (`project/mod.rs:1-13`) and Part B deliberately
keeps hook discovery *out* of it (a value, not a dependency — B.2 DI note). For
symmetry, a small `ProjectConfigResolver { repo_root, config_dir }` owns the layered
FS reads + the A.2.1 guards, and is used by **both** the `project.*` handlers (Part
A) and the hook dispatchers (Part B). The handlers resolve the project via
`ProjectManager` (for `repo_root`) and delegate config/prompt resolution to the
resolver; `api/handler.rs` mirrors the existing project handlers.

## A.4 Rendering split (provider data stays caller-side)

The daemon resolves the prompt **template** (with `${title}`/`${body}`/`${branch}`/…
placeholders) but does **not** render it: rendering needs provider data (issue/PR
JSON), and provider credentials live with the **caller** (the rofi client's
`linear`/`gh` today; the browser's aggregator backend in Phase 4 Slice E) — never in
the daemon. So:

1. Caller asks the daemon `project.action <project> <action>` → gets the recipe +
   the resolved prompt template content.
2. Caller fetches provider data with its own creds and renders the template with the
   **unchanged** single-pass, unknown-variable-rejecting renderer
   (`pohunek_render_provider_prompt`, `lib.sh:106`) — the
   "provider-controlled `${var}` is never re-expanded" guarantee is preserved.
3. Caller derives the branch (from `branchName`/`headRefName`) and runs
   `pohunek session new --agent … --project … --branch … --input <rendered>` using
   the recipe's `agent`/`base_branch`.

The `${var}` contract (`provider, id, number, title, body, branch, url`) is part of
the prompt-template spec, documented once and shared by every renderer (rofi now,
browser later), so the two clients render identically.

> The launcher scripts (`scripts/`) thin out to consumers of `project.action`; the
> host-wide `launcher.conf` keys that are **not** per-project (e.g. `terminal`,
> `rofi_bin`, `list_timeout_seconds`, `linear_cli`) stay client-side config. A
> future `session new --action <name>` that has the daemon render (once providers
> are daemon-reachable) is noted in [Open questions](#open-questions), not v1.

## A.5 What an in-repo definition may set (security)

Because there is **no trust gate** (Decision 5) and the daemon honors in-repo
definitions, an in-repo `templates.toml`/`actions.toml` from an **untrusted** repo
must not be able to control **what the daemon execs**. The boundary:

- **May set** (safe — all are *names* or already-validated launch inputs): `agent`
  (an agent **name** — a base kind or a host profile, resolved fail-closed; the repo
  *selects*, never *defines* — see Part C), `base_branch`, `branch` rule, and a
  `prompt` **name** (resolved fail-closed to a template *string* fed as `--input`,
  never executed).
- **May NOT set from the in-repo layer:** an arbitrary `program`, arbitrary agent
  `argv`/flags, or arbitrary `env` injected into the agent process. **These live
  exclusively in a host agent profile (Part C)** — the operator's own
  `~/.config/pohunek/agents/`, never honored from a checked-out repo.

This keeps the daemon-served-from-repo model within the same blast radius as the
launcher already had (a repo could already supply a Linear/PR branch name and body),
plus the (constrained) choice of an agent **name** the host pre-approved — and **no
further**. **Part C is the realization of this boundary's "host-config-only" half:**
the `program`/`argv`/`env` A.5 forbids from repos are exactly what a host-authored
profile legitimately provides, and an in-repo `agent = "<name>"` is a name lookup
against the host's profile set (same single-segment charset guard as A.2.1), not a
definition.

---

# Part B — Worktree lifecycle hooks (daemon-side)

Symmetric with Part A: same in-repo `.pohunek/` + host `~/.config/pohunek/` layering,
resolved by the daemon. The difference is kind (executed scripts, not declarative
config) and failure mode (a missing hook is skipped; a missing prompt is an error).

## B.1 The hook set

React-only scripts the daemon runs at lifecycle points. Per the design decision,
**v1 hooks never alter behavior** — they observe and cause side effects; the
operation proceeds regardless of hook outcome (failure → non-fatal warning).

| Hook | Fires at (file:line) | cwd | Context (env) | Typical use |
|---|---|---|---|---|
| `pre-create` | `worktree/mod.rs:256` (fresh-create path only, before dirs/git) | repo root | repo, branch, base, project, session, agent | pre-flight checks/log |
| `post-create` | `worktree/mod.rs:269` (after `git worktree add`, where `setup` runs now; **before** the binding is persisted at `:284`) | worktree | + worktree path, base actually used | install deps, scaffold, start services — **replaces/extends `.pohunek/setup`** |
| `pre-remove` | `worktree/mod.rs:333` (`cleanup_session`) / `:388` (`cleanup_project` prune) | worktree | full binding fields | push/archive uncommitted work, stop services |
| `post-remove` | `worktree/mod.rs:341` / `:399` (after `git worktree remove`) | **`binding.repository`** (the surviving source repo — the worktree is gone) | binding fields | external cleanup |
| `session-start` | session layer, after PTY spawn succeeds (`session/mod.rs` create path) | session cwd | + agent, session id | start port-forward / sidecar |
| `session-stop` | **`record_exit` (`session/mod.rs:1980`)** — the universal session-end point (covers `stop()` and natural exit) | session cwd | session id, agent, **`POHUNEK_STOP_REASON`** (stopped/done/failed) | stop sidecar (branch on reason) |
| `agent-state` | event dispatcher on an `event::AGENT_STATE` **value change** (`events/mod.rs`) | session cwd | session id, `activity` (`working`/`blocked`/`idle` — the existing `AgentActivity`) | notify, badge, auto-action |

### Lifecycle model (important, easy to get wrong)

- **A stopped session's worktree is retained.** `record_exit` (`session/mod.rs:1980`,
  reached by both `stop()` and the natural-exit watcher) flips state, persists the
  resume binding, emits `SESSION_STOPPED`, and **deliberately keeps the worktree**.
  So `session-stop` is **not** a worktree-removal hook and is decoupled from
  `pre/post-remove` — stopping a session fires `session-stop` only; no remove hook.
- **`pre/post-remove` fire only when a worktree is actually removed**, i.e. inside
  `cleanup_session` (launch-failure rollback, `session/mod.rs:880`) and
  `cleanup_project` (`project rm --prune-worktrees`). On the rollback path,
  `post-create` and then `pre-remove` run **back-to-back** on the same worktree —
  hooks must tolerate this (see [Edge cases](#edge-cases)).
- **`session-stop` fires on every terminal reason** — Stopped, Done, **and** Failed
  (`record_exit` terminal branch, `:2000`). The reason is passed as
  `POHUNEK_STOP_REASON` so a "stop my sidecar" script can branch. A **daemon crash
  bypasses `record_exit` entirely**, so no `session-stop` fires on crash —
  best-effort-on-clean-teardown; crash-leak cleanup is out of scope (see Risks).

**`.pohunek/setup` is the legacy alias for the *in-repo* `post-create` slot only** —
it does not interact with the host-global layer. Precisely: the `post-create` event
composes the host-global hook (B.2, runs first) **then** the *in-repo post-create
slot*, and that in-repo slot = `.pohunek/hooks/post-create` if present, **else**
`.pohunek/setup` (never both — that would double-run create-time side effects). The
full matrix (✓ = runs, in order):

| host `hooks/post-create` | repo `hooks/post-create` | repo `setup` | runs |
|:--:|:--:|:--:|---|
| ✓ | – | – | host |
| – | ✓ | – | repo post-create |
| – | – | ✓ | repo setup |
| ✓ | – | ✓ | host, then repo setup |
| – | ✓ | ✓ | repo post-create (setup ignored) |
| ✓ | ✓ | ✓ | host, then repo post-create (setup ignored) |

So `setup` is shadowed only by the *in-repo* `post-create` hook, never by the
host-global one. No migration needed (experimental project).

## B.2 Hook discovery & layering

Hooks are filesystem-discovered (no protocol, no store), the same `.pohunek/` +
`~/.config/pohunek/` layering as Part A, resolved **on the daemon's host**:

```
<repo>/.pohunek/hooks/<event>            # in-repo, travels with the repo
~/.config/pohunek/hooks/<event>          # host-global default (on the daemon host)
```

For a given `<event>`, both layers' scripts run when present, **host-global first,
then in-repo** (they *compose* — for side-effect scripts "run all" is more natural
than "override"). This compose rule is **per event name** and is distinct from the
`setup`→`post-create` rule in B.1, which is a *precedence/fallback* (only one runs).
A directory form `<event>.d/*` (sorted order) is a possible later refinement; v1
ships the single-file form. `<event>` ∈ the seven names in B.1.

> **DI note.** `WorktreeManager` already holds the repo + worktree paths, so it
> discovers `<repo>/.pohunek/hooks/` itself — no `ProjectManager`/`ProjectRecord`
> access needed (hooks are FS-discovered, not store-driven). The **host-global
> `~/.config/pohunek/hooks/` layer is not derivable from worktree paths**, so the
> daemon's resolved XDG config dir is **threaded as a plain config input** into
> `WorktreeManager::new` (`session/mod.rs:502`) and the session/event-layer
> dispatchers — a value, not a `ProjectManager` dependency. `session-start/stop` and
> `agent-state` hooks resolve the in-repo layer from the session cwd.

## B.3 Execution discipline (reuse, don't reinvent)

**All seven hooks route through one `run_hook(event, ctx, timeout, warnings)`
helper** (the generalization of `run_setup_script`, `worktree/mod.rs:827`), so the
discipline is enforced in **one place** for every event:

- launched via `sh <script>` so a non-executable committed file still runs;
- in its **own process group** so a timeout kills the whole subtree
  (`terminate_setup_script`, `:928`);
- **bounded by a timeout** (generalize `setup_script_timeout` to a per-hook
  `hook_timeout` in `SessionRegistryConfig`, `session/mod.rs:126`);
- **stdout/stderr discarded to `/dev/null`** — no hook *output* ever reaches
  `SessionWarning` / the append-only event log (`events/mod.rs`);
- **environment is `.env_clear()`-ed, then an explicit allowlist is set** — only
  `PATH`, `HOME`, and the documented `POHUNEK_*` context vars. **New and
  load-bearing:** the daemon today spawns the setup script with its **full inherited
  environment** (`run_setup_script` sets only arg/cwd/stdio/process-group), so a hook
  inherits `GITHUB_TOKEN`, `ANTHROPIC_API_KEY`, `POHUNEK_SOCKET_PATH`, etc. With no
  trust gate, a hostile repo's hook could exfiltrate those (`/dev/null` only hides
  *output*, not the hook's own outbound `curl`). Clearing the env is the **primary**
  secret-safety control; the discard rule complements it. See
  [Security & trust](#security--trust).
- failure/timeout/spawn-error → a **non-fatal** `SessionWarning`. `SessionWarningKind`
  serializes today as a bare string (unit variants `fetch`/`base_branch_fallback`/
  `setup_script`, `protocol/src/session.rs:278`; CLI maps each to a label,
  `cli/src/commands/session.rs:795`), so the new kind is a **unit variant `Hook`**
  (serializes `"hook"`) — **not** a struct variant `Hook { event }`, which would break
  that string shape. The failing event name (`post-create`, `pre-remove`, …) goes in
  the existing `SessionWarning.message`/`detail` (e.g. "post-create hook failed: …"),
  not in the kind. (If a machine-readable event field is wanted, add one optional field
  to `SessionWarning` and roundtrip it — but the kind stays a string.)

The `agent-state` hook runs off the event-log drain's hot path: it subscribes to the
same broadcast the event log drains (`events/mod.rs:114` `spawn_drain` pattern) on
its **own task**, so a slow hook can never wedge the audit log. The dispatcher
**tracks the last-fired activity per session** and fires only when the activity
**value actually changes** — `record_activity` (`session/mod.rs:1951`) and the
detector's periodic refresh (`detect/machine.rs:111` `tick()`) re-publish the *same*
visible state, so a naive subscriber would fire on every refresh tick. Last-fired
tracking drops same-state re-emissions; a short time-debounce smooths genuine flap.

### B.3.1 Hook environment contract (normative)

Every hook is invoked with cwd set per the B.1 table and the cleared+allowlisted env
below. All values are non-secret. A var is **present** only for the events marked;
absent otherwise (scripts must tolerate unset, e.g. `${POHUNEK_BASE_BRANCH:-}`).

| Var | Value | Present for |
|---|---|---|
| `POHUNEK_HOOK_EVENT` | the event name (`pre-create`/`post-create`/`pre-remove`/`post-remove`/`session-start`/`session-stop`/`agent-state`) | all |
| `POHUNEK_SESSION_ID` | the session id | all |
| `POHUNEK_PROJECT_ID` | the `p-…` project id (empty if none) | all |
| `POHUNEK_AGENT` | the resolved agent name | all |
| `POHUNEK_REPO` | absolute path of the source repository | create/remove |
| `POHUNEK_WORKTREE` | absolute worktree path | post-create, pre-remove (cwd is the worktree) |
| `POHUNEK_BRANCH` | the worktree branch | create/remove |
| `POHUNEK_BASE_BRANCH` | resolved base branch | post-create |
| `POHUNEK_STOP_REASON` | `stopped` \| `done` \| `failed` | session-stop |
| `POHUNEK_ACTIVITY` | `working` \| `blocked` \| `idle` (the existing `AgentActivity`, `protocol/src/session.rs:28` — no `done`/terminal value today) | agent-state |

This list is the testable contract (a DoD asserts the exact set per event); new vars
are additive. It is **disjoint from** the launch-time `POHUNEK_*` handshake env
(`session_pty_env`) — hooks never receive `POHUNEK_SOCKET_PATH`/`_DAEMON_ID`/`_ENV`/
`_PROTOCOL_VERSION`.

## B.4 Semantics (resolved for v1, open to review)

- **React-only, never veto.** No hook can abort an operation in v1 (honors the "jen
  reagovat" decision). A failing `pre-create` does **not** stop creation; a failing
  `pre-remove` does **not** stop removal. (A future opt-in "blocking" hook with a
  defined error class is noted in [Open questions](#open-questions).)
- **Create hooks fire on fresh-create only, not reuse.** Matches the setup script,
  which never runs on a reused worktree (`worktree/mod.rs:216` early-returns before
  `:269`).
- **Remove hooks are best-effort.** `cleanup_session`/`cleanup_project` already drop
  the binding even when `git worktree remove` fails; hooks fit that contract and
  never block binding cleanup.
- **`post-create` must be idempotent / tolerate a hook-silent rollback.** The
  `post-create` seam (`:269`) runs **before** the binding is persisted (`:284`); if
  the persist fails the worktree is rolled back via `worktree_remove` (`:290`)
  **without firing any remove hook**. So a `post-create` side effect can leak; v1
  requires `post-create` effects to be idempotent/self-healing. Moving the seam to
  after `:284` is an alternative noted in [Open questions](#open-questions).
- **No `worktree.*` protocol method is introduced.** Worktrees are still created only
  as a side effect of `session.new`; hooks fire inside that flow.

---

# Part C — Per-host agent profiles (daemon-side)

Today an "agent" is a **closed 3-variant enum + two compile-time adapters**, not data:
`AgentKind {Shell,Codex,Claude}` (`protocol/src/session.rs:14`) dispatches by
hand-written `match` to zero-sized `ClaudeAdapter`/`CodexAdapter`
(`agent/claude.rs`, `agent/codex.rs`) at four sites — launch
(`session/mod.rs:2106`), input rules (`:2134`), detection (`detect/mod.rs:53`), resume
(`agent/mod.rs:224`). Each adapter hardcodes a `&'static str` program, empty argv,
`InputRules`, a `&'static Manifest`, and a resume-argv template. **The `&'static`
program is the security boundary: only `claude`/`codex`/`$SHELL` can ever be exec'd,
and no socket string can pick the program** (`agent/mod.rs:186`, `resolve_binary`
`:248`).

Per-host **agent profiles** turn that hardcoding into host-authored data, following
Kandev's split: a **base kind stays compiled-in** (Decision: *extend*, not replace),
and a profile *extends* one with overrides.

> **Prerequisite refactor (decided): a `ShellAdapter`.** Today Shell is *not* an
> `AgentAdapter` — it is special-cased to `ShellCommand` in `build_launch_command`
> (`session/mod.rs:2106`) with inline input rules (`:2134`). To make "all three base
> kinds resolve through one data-driven path" literally true, Part C first introduces
> a `ShellAdapter` (wrapping today's `ShellCommand` defaults: `$SHELL`/`/bin/sh`,
> `{bracketed_paste:false, submit_delay:0}`, the generic-shell manifest,
> not-resumable). Then the four `match` sites collapse uniformly. A `base = "shell"`
> profile therefore overrides the shell adapter's program/args/env/input-rules like
> any other; it always **forces `resumable = false`** (a shell has no native resume),
> rejected at load if a profile sets `resumable = true` on a shell base.

## C.1 Profile model & file

A profile is a host-authored TOML file at `~/.config/pohunek/agents/<name>.toml` on
the **daemon's host** (the profile *name* is the file stem). It declares a **base
kind** it inherits detection + resume + input-rule defaults from, and overrides only
what it needs (Decision: *inherit base + optional override*):

```toml
# ~/.config/pohunek/agents/claude-sonnet.toml
base = "claude"                 # base kind: shell | codex | claude
                                #   -> inherits manifest + resume template + input_rules
program = "claude"              # PATH name or absolute path — HOST-ONLY (A.5's forbidden-from-repo field)
args = ["--model", "claude-sonnet-4", "--add-dir", "/shared"]

[env]                           # merged into the PTY env; reserved POHUNEK_* keys always win (C.5)
ANTHROPIC_MODEL = "claude-sonnet-4"

[input_rules]                   # optional; else the base kind's defaults
bracketed_paste = false
submit_delay_ms = 150

[resume]                        # optional; else inherited from base
mode = "flag"                   # flag => ["--resume", <ref>] ; subcommand => ["resume", <ref>]
ref_kind = "id"                 # id | path  (selects SessionRef::id vs ::path validation)
resumable = true

# detection: inherit the base kind's embedded manifest by default, OR point at a
# host manifest parsed via the CAPPED Manifest::parse_str (never the .expect path):
# manifest = "claude-custom"    # ~/.config/pohunek/agents/manifests/<name>.toml
```

The three built-in names (`shell`/`codex`/`claude`) with **no** profile file behave
**exactly as today** (zero change for existing callers). A profile file named after a
base kind overrides that kind's defaults.

## C.2 Resolution & the wire (free-string `agent`)

`agent` on the wire becomes a **free string name** (Decision: free string), resolved
**daemon-side on the target host**:

1. Validate the name with the **A.2.1 single-segment charset guard**
   (`^[A-Za-z0-9._-]+$`, no `/`/`..`/control) — `invalid_name`.
2. If `~/.config/pohunek/agents/<name>.toml` exists → that profile.
3. Else if `<name>` ∈ {`shell`,`codex`,`claude`} → the bare base kind (today's
   behavior).
4. Else → **hard `agent_profile_not_found`** (no silent fallback — the fail-closed
   posture). The wire string is therefore always a **name**, never a program; the
   exec'd program comes only from a base kind (compiled) or a host profile
   (operator-owned) — never from the wire or a repo.

The same name appears in an in-repo `template.agent` (Part A) — same guard, same
fail-closed resolution against the **target host's** profile set.

## C.3 Detection & resume inheritance

- **Detection:** inherit the base kind's `include_str!`-embedded `&'static Manifest`
  (`detect/mod.rs:82`) unless the profile sets `manifest = "<name>"`, loaded from
  `~/.config/pohunek/agents/manifests/<name>.toml` via the **capped, non-panicking**
  `Manifest::parse_str` (`detect/manifest.rs:22`, already hardened: MAX_RULES=128,
  MAX_DEPTH=8, 256 KiB cap). The `manifest` name uses the **same A.2.1 single-segment
  charset + canonicalize-and-contain guard** as prompt/agent names (it joins into a
  path). A malformed host manifest **disables that one profile**, it does not
  `.expect`-panic the daemon.
- **Resume:** inherit the base's argv template, or override `resume.mode`/`ref_kind`/
  `resumable`. `ref_kind` picks the validating `SessionRef` ctor (`agent/mod.rs:56`),
  preserving the leading-dash argv-injection guard. A non-resumable profile yields the
  existing typed `agent_not_resumable`.
- **`report_native_id` resolves `ref_kind` from the session's launch-time profile, not
  the wire.** The agent's SessionStart hook reports its native id from the handshake
  env, which carries **no profile identity** (the Claude hook hardcodes `"claude"`,
  Codex `"codex"` — `integration/assets/claude/pohunek-agent-state.sh:41`); so the
  daemon decides id-vs-path by looking up the **session's launch-time profile by
  `session_id`** (which it already has, `session/mod.rs:1295`), **not** from the wire
  field. `SessionReportNativeIdParams.agent` (`protocol/src/session.rs:189`) **does**
  migrate to a free string with every other `agent` field (uniform enum→string change,
  no special case), but its **value** is just the base-kind name the hook emits and is
  **not load-bearing** — the daemon ignores it for `ref_kind`. This reconciles the
  field's type (migrated) with its role (informational only).

## C.4 Snapshot launch-affecting fields onto the resume binding

A session's `ResumeBinding` (`store/mod.rs:42`) persists the agent; a profile can be
**edited or deleted** between the session's start and a daemon-restart resume.
Following Kandev's snapshot pattern, **freeze the structural relaunch fields at session
creation** so a post-start profile edit cannot break an in-flight resume:

- **Snapshotted (frozen, persisted):** `program`, `args`, `input_rules`, the resume
  template + `ref_kind` + `resumable`, the resolved `base` kind (the manifest is
  re-derived from the base, or its name re-resolved). These are **non-secret** and
  define how the session relaunches.
- **NOT snapshotted: `env`.** Profile `env` is **re-resolved from the profile by name
  at resume** (not frozen), for one reason: `env` is the only field that *could* carry
  a secret, and the metadata store must keep its **no-secrets invariant** (§Storage —
  the store, like the event log, never holds a secret). The accepted trade-off: if a
  profile's `env` is edited between launch and a daemon-restart resume, the resumed
  session picks up the **new** `env`; if the profile is gone, it resumes with **no
  profile env + a warning**. (Within a single run — no restart — env was already
  applied at launch and the live process keeps it.)

A *deleted* profile still relaunches from the frozen structural snapshot; only a
genuinely missing snapshot fails (gracefully, typed — never a panic).

> **Profile `env` is non-secret config by contract** (model ids, feature flags,
> `--add-dir` targets, …). **Secrets continue to come from the daemon's inherited
> environment**, exactly as today (the agent PTY inherits the daemon env;
> `GITHUB_TOKEN`/`ANTHROPIC_API_KEY` reach the agent that way, never via a profile).
> The loader **must reject/strip a profile `env` key that collides with the daemon's
> own secret-bearing vars** is out of scope to enumerate, but the store never persists
> profile `env` regardless — so even a misconfigured profile cannot leak a secret into
> on-disk state.

**Where the snapshot lives matters:** `persist_resume_binding` rebuilds the
`ResumeBinding` from the in-memory session entry on **every** call (creation,
`report_native_id`, resize — `session/mod.rs:1448-1493`). The snapshot must be stored
**on the in-memory session entry at creation and copied verbatim on every
re-persist** — it must **never be re-resolved from disk** on a later re-persist, or a
mid-session profile edit would overwrite the frozen values and re-open the very window
this closes.

## C.5 Security & remote

- **Program/argv/env come only from a base kind or a host profile.** The wire/in-repo
  `agent` is a name; `resolve_binary` still gates the program, now over the
  operator-defined set. Profiles are read **only** from `~/.config/pohunek/agents/`
  on the daemon host — never from a repo or the wire.
- **The whole `agents/` tree is ownership/permission-gated, not just each file**
  (like `~/.ssh`): this layer defines exec'd programs, so a world-writable
  `~/.config/pohunek/agents/` is itself a privilege path (an attacker could replace a
  profile with a symlink to `/attacker/profile.toml`). The loader verifies that
  `agents/` and `agents/manifests/` are **owned by the daemon user and not
  group/world-writable** (else the whole dir is skipped + warned), and **reuses the
  A.2.1 canonicalize-and-contain guard**: each resolved `agents/<name>.toml` (and
  manifest file) is canonicalized and must stay within the `agents/` tree, so a
  committed/substituted symlink cannot escape to an arbitrary file. Owner-checking the
  file alone (an `lstat` that execs from a link target) is **insufficient** — guard
  the dir + containment.
- **Every `POHUNEK_`-prefixed env key is reserved; the daemon's env wins.** A profile's
  `[env]` is merged **before** `session_pty_env` (`session/mod.rs:1222`) appends the
  handshake vars, so last-write-wins makes the daemon authoritative. The reserved set
  is the **whole `POHUNEK_` prefix** — all five today (`POHUNEK_SESSION_ID`,
  `POHUNEK_DAEMON_ID`, `POHUNEK_SOCKET_PATH`, `POHUNEK_ENV`, `POHUNEK_PROTOCOL_VERSION`)
  and any future one — not a hand-listed three; a profile setting `POHUNEK_ENV=0`
  (which would silently kill SessionStart native-id capture → no resume) or corrupting
  `POHUNEK_PROTOCOL_VERSION` must be impossible.
- **Remote = fail-closed, never ship a profile body over the wire.** A profile name is
  resolved against the *target* daemon's profile set; an unknown name →
  `agent_profile_not_found`. `host.inspect` enumerates available profile names so a
  client can discover them first. A client **may not** send a profile *definition*
  over the wire — that would re-open A.5 (an off-host source defining
  `program`/`argv`/`env`); profiles are host-authored only.

---

## Protocol & daemon changes (touch-points)

**Prerequisite (both parts depend on it): the daemon has no config dir today.**
`Paths::resolve` (`crates/daemon/src/paths.rs:50-75`) derives runtime/socket/lock/
log/data dirs but **never reads `XDG_CONFIG_HOME`** — `~/.config/pohunek` is a
CLI-only path (`crates/cli/src/paths.rs`). The host-default layer (and the hook
host-global layer, B.2) needs a daemon-side config dir, so first: extend
`Paths::resolve` to derive `config_dir` from `XDG_CONFIG_HOME` (fallback
`HOME/.config`) + `APP_DIR`, **failing fast if neither is available** (no silent
default); add `config_dir` to `SessionRegistryConfig` (`session/mod.rs:126`), set it
in `main.rs:77`. This is **Slice 0**; A1, B3, and C1 block on it.

Part A (daemon-served definitions):

- `crates/protocol/src/lib.rs`: add `PROJECT_ACTIONS`/`PROJECT_ACTION`/`PROJECT_PROMPT`
  method consts (additive; no version bump). `crates/protocol/src/project.rs`: new
  `*Params`/`*Result` structs (resolved recipe + prompt content; typed error codes
  `prompt_not_found`, `action_not_found`, `template_not_found`, `invalid_name` (shared
  bad-name code), `invalid_action`, `invalid_template`).
- New `ProjectConfigResolver { repo_root, config_dir }` (daemon) owning the layered FS
  reads + the A.2.1 name/path/containment guards
  (`resolve_prompt`/`resolve_action`/`list_actions`/`resolve_template`).
  `crates/daemon/src/api/handler.rs`: three handlers resolve the project via
  `ProjectManager` (for `repo_root`) then delegate to the resolver; they mirror the
  existing project handlers.
- `crates/cli/src/commands/project.rs` + `crates/cli/src/main.rs:129` `ProjectAction`:
  add `prompt`/`actions`/**`action`** subcommands (human + `--json`, `--host` routing)
  alongside `List`/`Add`/`Show`/`Rename`/`Rm`. `project action` (singular) is what the
  launcher calls.
- `scripts/`: `pohunek-launch-issue`/`-pr` call `project action` for the recipe +
  prompt, then fetch+render+`session new` (A.4). Host-only keys stay in client config.

Part B (hooks):

- `crates/daemon/src/worktree/mod.rs`: generalize `run_setup_script` → one
  `run_hook` (`.env_clear()` + allowlist + process-group + timeout + `/dev/null`);
  call sites at `:256`/`:269`/`:333`/`:341`/`:388`/`:399`. **`WorktreeRequest`
  (`:68`) must gain the fields the B.3.1 env needs that it lacks today — notably the
  resolved `agent` name** (it carries session/repo/branch/base/project_id but **not**
  the agent); `bind_worktree` (`session/mod.rs:1072`) threads the agent name in.
  Otherwise `POHUNEK_AGENT` cannot be set for `pre/post-create`/`pre/post-remove`.
- `crates/daemon/src/session/mod.rs`: fire `session-start` after spawn; `session-stop`
  in `record_exit` (`:1980`) with `POHUNEK_STOP_REASON`; thread `hook_timeout` + XDG
  config dir from `SessionRegistryConfig` (`:126`) into `WorktreeManager::new`.
- `crates/daemon/src/events/` or `session/`: an `agent-state` dispatcher subscribing
  to `event::AGENT_STATE`, per-session last-fired tracking.
- `crates/protocol/src/session.rs:278`: add `SessionWarningKind::Hook` as a **unit
  variant** (serializes `"hook"`, like the existing three); the event name rides in
  `SessionWarning.message`/`detail`, not the kind. Label in
  `cli/src/commands/session.rs:795`. `crates/daemon/src/main.rs`: `hook_timeout` (+ XDG
  config dir) into `SessionRegistryConfig` (`:77`).

Part C (agent profiles):

- `crates/protocol/src/session.rs`: `agent` becomes a **free string** (the agent
  name) on `SessionNewParams.agent` (`:41`), `SessionInfo.agent` (`:313`),
  `SessionReportNativeIdParams.agent` (`:193`), `SessionListFilter::Agent` (`:96`);
  `HostCapabilities.supported_agents`/`AgentRuntime.agent` (`capabilities.rs:28,47`).
  The unit-enum `AgentKind` stays as an internal **base-kind** type (and the roundtrip
  tests at `tests/roundtrip.rs:59` move to the string shape). New typed errors
  `invalid_name`/`agent_profile_not_found`. **`SessionInfo` carries both the agent
  name and its resolved base kind** (the base is part of the launch-time snapshot, C.4).
  **`SessionListFilter::Agent` groups by the SNAPSHOTTED base kind**, not a live
  profile read: `agent=claude` matches a session whose stored agent name is `claude`
  **or** whose stored base kind is `claude`. Resolving from the snapshot (not
  re-reading the profile at list time) keeps filtering **stable after a profile is
  edited/deleted** — consistent with C.4.
- Introduce a `ShellAdapter` (the decided refactor) so all three base kinds are
  `AgentAdapter`s and the special-case at `build_launch_command` (`session/mod.rs:2106`)
  / `input_rules_for_agent` (`:2134`) is removed. New `AgentProfile` loader +
  `ResolvedProfile { program, args, env, input_rules, manifest, resume_template,
  resumable }` (daemon). `AgentAdapter` (`agent/mod.rs:173`) relaxes `id()->&str` /
  `manifest()->&Manifest` and the launch program from `&'static str` (`:186`) to owned
  data; the four `match` sites (`session/mod.rs:2106`, `:2134`, `detect/mod.rs:53`,
  `agent/mod.rs:224`) collapse uniformly to "resolve profile → build from data".
  `resolve_binary` (`agent/mod.rs:248`) keeps PATH resolution over the now
  operator-defined program.
- `crates/daemon/src/store/mod.rs:42` `ResumeBinding`: persist the agent **name** +
  the **structural** snapshot (program/args/input_rules/resume
  template+ref_kind+resumable/base **— NOT `env`**, C.4) + the resolved **base kind**
  (for the filter, see below), held on the in-memory session entry and copied verbatim
  by `persist_resume_binding` (`session/mod.rs:1448`), never re-resolved; profile `env`
  is re-resolved at resume, never stored. Resume tolerates a deleted/edited profile
  (typed error, no panic).
  `report_native_id` (`session/mod.rs:1295`) resolves id-vs-path from the session's
  launch-time profile (by `session_id`), **not** from the wire `agent` field, which
  stays the base-kind identity the hook reports.
- `crates/daemon/src/capabilities.rs:20`: enumerate loaded profile names + probe each
  `program`, so `host.inspect` lists available agents.
- `crates/daemon/src/integration/mod.rs:73`: a profile inheriting `base = "claude"`
  gets the claude SessionStart hook; hook opt-in inherited from the base kind.
- Profile **loader** at boot: a profiles-dir (`config_dir/agents`) on
  `SessionRegistryConfig` (`session/mod.rs:124`), loaded/validated in `main.rs:77`
  (owner-only permission gate, C.5), passed into `SessionRegistry::new`.
- `crates/cli/src/commands/session.rs:28`: `--agent` accepts a free-form name;
  `agent_label` (`:746`) handles arbitrary names; `parse_agent_filter` (`:195`) too.

## Storage

- Part A definitions: files only — in-repo `.pohunek/{templates,actions}.toml` +
  `prompts/`, and host `~/.config/pohunek/` on the daemon host. The daemon **reads**
  them; nothing in the metadata store, nothing persisted on the wire.
- Part B hooks: files only (`.pohunek/hooks/`, `~/.config/pohunek/hooks/`).
- Part C profiles: files only — `~/.config/pohunek/agents/<name>.toml` (+
  `agents/manifests/`) on the daemon host, owner-only. Persisted in `ResumeBinding`:
  the agent **name** + the C.4 **structural** snapshot (program/args/input_rules/
  resume-template+ref_kind+resumable/base) — **never the profile `env`** (re-resolved
  at resume, C.4) and never the profile body. This preserves the store's no-secrets
  invariant: profile `env` is the only potentially-secret field and it never reaches
  disk.

Definitions/hooks/profiles are all FS config (not the metadata store, `store/mod.rs`),
so all three are **upgrade-safe** (the store has no back-compat guarantee and may be
wiped on upgrade). Only the non-secret structural resume snapshot rides in the store —
consistent with the existing invariant that the store and event log hold no secrets.

## Edge cases

- **Non-git session / no worktree:** no `.pohunek/` → no hooks, no in-repo
  definitions; host defaults only. A requested-but-missing prompt is still an error.
- **In-place session (`session new` without `--branch`, `session/mod.rs:977`):** cwd
  = checkout root, which still has `.pohunek/` → `session-start/stop` and
  `agent-state` hooks fire; **no `pre/post-create` or `pre/post-remove`** (no worktree
  created/removed). In-place sessions get only **half** the hook surface and run
  against the **shared checkout** with no per-session isolation, so **two concurrent
  in-place sessions share one cwd** and their `session-start` hooks race. Documented;
  not solved in v1.
- **Worktree reuse:** create hooks skipped (B.4); `session-start` still fires.
- **Launch-failure rollback:** `post-create` fires, then the binding persist fails
  and the worktree is removed **without** a `pre/post-remove` hook (B.4) — so
  `post-create` effects must be idempotent/self-healing.
- **`post-remove` cwd:** resolved from `binding.repository` (the worktree is gone); if
  `binding.repository` no longer exists, the hook is **skipped with a warning**.
- **Hook timeout / crash:** killed via the process-group path; non-fatal warning;
  worktree kept / removal proceeds.
- **`agent-state` re-emission:** the detector republishes the same state every refresh
  tick; the dispatcher fires only on an actual value change, plus a flap debounce.
- **`session-stop` reason / daemon crash:** fires on stopped/done/failed with
  `POHUNEK_STOP_REASON`; a daemon crash fires nothing.
- **Prompt missing:** `project.prompt`/`project.action` return a typed
  `prompt_not_found` error the caller surfaces; the launcher aborts without starting a
  session (no silent fallback).
- **Remote project:** fully supported — the daemon resolves in-repo `.pohunek/`
  locally on its own host. (The former client-side remote limitation is gone.)
- **Secret in hook output / env:** the daemon discards hook stdout/stderr to
  `/dev/null` and clears the hook env to an allowlist (B.3), so neither hook output
  nor ambient daemon credentials leak through the daemon.
- **Unknown agent name:** `session.new`/a template naming an agent with no matching
  profile or base kind on the target host → `agent_profile_not_found` (fail-closed, no
  silent default), surfaced to the caller. A bare `shell`/`codex`/`claude` always
  resolves (base kind).
- **Profile edited/deleted between launch and resume:** resume uses the C.4 snapshot
  on the `ResumeBinding`, so an edit can't break an in-flight resume; a deleted
  profile resumes from the snapshot. Only a missing snapshot fails (typed, no panic).
- **Non-owner-only profile file or `agents/` dir:** skipped with a warning (C.5), not
  honored — it is the layer that defines exec'd programs; a symlink escaping the
  `agents/` tree is rejected (A.2.1 containment).

## Slices & Definition of Done

Lettered, independently shippable; each ends green (`cargo test --workspace`,
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`).

> **Slice labels match Part labels:** `A*` = Part A (actions/prompts), `B*` = Part B
> (hooks), `C*` = Part C (profiles). Slice 0 is the shared prerequisite.

- **Slice 0 — Daemon config dir (shared prerequisite).** Extend `Paths::resolve` with a
  fail-fast `config_dir` (`XDG_CONFIG_HOME` → `HOME/.config` → error); add it to
  `SessionRegistryConfig` and thread it. *DoD:* the daemon resolves
  `~/.config/pohunek` (or `XDG_CONFIG_HOME`), and fails fast when neither is set.
- **Slice A1 — `project.prompt` resolution + CLI (Part A core).** A
  `ProjectConfigResolver` doing layered fail-closed prompt resolution **with the A.2.1
  charset + symlink-containment guards (for the TOMLs too, not just prompts)**;
  `project.prompt` method + `pohunek project prompt`. *DoD:* an in-repo
  `prompts/<name>.tmpl` wins over the host default; a missing name returns
  `prompt_not_found`; a name with `..`/`/` returns `invalid_name` (traversal
  test); a `prompts/x.tmpl`, a `templates.toml`, **and** an `actions.toml` symlink
  pointing outside `.pohunek/` are each rejected (symlink-escape test, all three read
  surfaces); works for a remote project over `--host`; `--json` shape covered.
- **Slice A2 — `project.action`/`project.actions` + templates (Part A).** Template +
  action resolution (the A.1 schema, per-name layer-wins, typed
  `action_not_found`/`template_not_found`/`invalid_*`), recipe assembly, the A.5
  safe-subset enforcement. *DoD:* an action resolves to a recipe carrying the resolved
  prompt content; an in-repo template selecting `codex` is honored; an in-repo template
  attempting to set a disallowed `program`/`env` is **rejected/ignored** (A.5 test); an
  action with an explicit `branch` under a non-`none` provider errors `invalid_action`;
  `project actions` lists resolvable actions.
- **Slice A3 — Launcher consumes the daemon (Part A).** `pohunek-launch-issue`/`-pr`
  call `project action`, fetch provider data, render with the unchanged renderer, and
  `session new` from the recipe. The client **`launcher.conf` stays flat `key=value`**
  (parser at `scripts/lib.sh:26` unchanged) but **loses the now-daemon-resolved keys**
  (`agent`/`project`/prompt selection), keeping only host-only keys; `pohunek setup`
  embeds the shrunken `launcher.conf`. **No client-side TOML** (the per-project/
  template/prompt TOML lives daemon-side, A.1/A.2). *DoD:* two projects launch issues
  with different agents/prompts driven entirely by daemon-resolved definitions; the
  flat-`launcher.conf` parser still reads the remaining host-only keys; the renderer's
  `${var}`/tab-flatten guards still hold (`scripts.rs`).
- **Slice B1 — Generalize the setup script into hooks (Part B core).** Replace
  `run_setup_script` with one `run_hook` helper (`.env_clear()` + allowlist +
  process-group + timeout + `/dev/null`); wire `pre/post-create`, `pre/post-remove`;
  `SessionWarningKind::Hook` (unit variant). *DoD:* a repo with
  `.pohunek/hooks/post-create` runs it on worktree create (warning on failure, worktree
  kept); `pre-remove` runs before `git worktree remove`; `post-remove` cwd is
  `binding.repository`; the **B.1 setup-vs-post-create matrix** holds (each of the six
  rows, test); a hook does **not** see a sentinel secret in the daemon's env
  (`.env_clear` test); hook env carries the **exact B.3.1 var set per event** (test);
  output never appears in the event log.
- **Slice B2 — Session-lifecycle & agent-state hooks (Part B).** `session-start`
  after spawn; `session-stop` from `record_exit` with `POHUNEK_STOP_REASON`;
  `agent-state` dispatcher with per-session last-fired tracking. All via `run_hook`.
  *DoD:* `session-start` runs after spawn; `session-stop` fires once per terminal exit
  with the reason; `agent-state` fires on working→blocked but **not** on same-state
  refresh ticks (test); a slow `agent-state` hook cannot stall the event log; env-clear
  + discard hold for all three.
- **Slice B3 — Host-global hook layer.** `~/.config/pohunek/hooks/<event>` threaded
  via the config dir; compose host-global then in-repo. *DoD:* both run, in order;
  either alone runs.
- **Slice C0 — `agent` becomes a free-string name + `ShellAdapter` (Part C protocol
  shift).** Change `AgentKind`-typed wire fields to a string; keep `AgentKind` as the
  internal base kind; introduce `ShellAdapter` so all three base kinds are adapters and
  the Shell special-cases at `build_launch_command`/`input_rules_for_agent` are
  removed; resolve a bare base-kind name exactly as today. *DoD:* `session new --agent
  claude/codex/shell` behaves unchanged; an unknown name with no profile returns
  `agent_profile_not_found`; a `../`/`/` name returns `invalid_name`; roundtrip
  tests updated to the string shape; `session list --filter agent=claude` matches a
  `claude-sonnet`-profile session (base-kind grouping test); `SessionInfo` shows the
  profile name.
- **Slice C1 — Profile loader + resolution + launch (Part C core).** `AgentProfile`
  loader (dir + file owner-only gate, A.2.1 containment) over `config_dir/agents`;
  `ResolvedProfile`; data-driven launch; inherit base kind, override program/args/env/
  input_rules. *DoD:* a `claude-sonnet` profile launches `claude` with the profile's
  args/env; a profile setting **any** `POHUNEK_*` key (incl. `POHUNEK_ENV`,
  `POHUNEK_PROTOCOL_VERSION`) cannot override the daemon value (test); a world-writable
  `agents/` **and** an `agents/x.toml` symlink escaping the dir are both refused
  (test); a non-owner-only profile is skipped + warned; `host.inspect` lists the
  profile.
- **Slice C2 — Detection/resume override + structural resume snapshot (Part C).**
  Optional host `manifest` (guarded name + capped `parse_str`); `resume.mode`/
  `ref_kind`/`resumable`; snapshot the **structural** fields (program/args/input_rules/
  resume/base) on the session entry, **excluding `env`** (C.4). *DoD:* a profile with a
  custom `manifest` detects state; a malformed host manifest disables only that profile
  (no panic); **after editing program/args/input_rules post-creation and forcing a
  resize re-persist, a restart-resume still uses the original structural values**
  (snapshot test); **the profile `env` is NOT written to the store** (secret-safety
  test) and is re-resolved at resume (a deleted profile → no profile env + warning); a
  `resumable=false` profile yields `agent_not_resumable`.

Build order: **Slice 0** (daemon config dir) first — A1, B3, **and C1** all need it;
then three parallel tracks that don't share files: A1 → A2 → A3 (Part A) ∥ B1 → B2 → B3
(hooks) ∥ C0 → C1 → C2 (profiles). C0 (the wire shift) is the riskiest single step
(touches every `AgentKind` site) and is best landed early and alone.

## Risks & mitigations

- **A checked-out repo influencing what the daemon execs (no trust gate).** With
  daemon-served definitions, an in-repo `templates.toml` is read by the daemon.
  *Mitigation:* the A.5 safe-subset — an in-repo template may only **name** an agent
  (a base kind or a host profile, resolved fail-closed), `base_branch`, branch rule,
  and a prompt *name* (fed as `--input`, never executed). `program`/`argv`/`env` come
  only from a base kind or a host profile (Part C), **never from the repo**. So the
  repo's power is no greater than the launcher already gave it, plus naming an agent
  the host pre-approved.
- **Free-string `agent` on the wire + operator-defined binaries (Part C).** `agent` is
  now a free string and profiles let the operator define `program`/`argv`/`env`.
  *Mitigation:* the wire string is a **name** (A.2.1 charset guard, fail-closed) that
  resolves only to a compiled base kind or a host profile — it can never *be* a
  program; the only new exec surface is the operator's own
  `~/.config/pohunek/agents/`, which is **owner-only permission-gated** (skipped +
  warned otherwise) and never accepted over the wire or from a repo. The wire caller
  is already trusted (single operator behind the NetBird boundary).
- **Arbitrary code execution via hooks (no trust gate).** Same posture as the existing
  `.pohunek/setup`, broadened to more events. *Mitigations:* `.env_clear()` to an
  allowlist (no inherited `GITHUB_TOKEN`/`ANTHROPIC_API_KEY`/`POHUNEK_SOCKET_PATH` —
  the biggest exfiltration vector), output discarded, timeout + process-group. The
  residual risk is arbitrary code running **as the daemon user** within the timeout; a
  trust gate is the recommended future hardening ([Open questions](#open-questions)),
  out of scope per the decision. The env-clear is the one hardening this RFC does
  **not** defer.
- **Daemon reads repo-named files and returns their bytes (path traversal / symlink
  escape).** New with the pivot — the launcher never did server-side reads of
  repo-named files. *Mitigation:* the A.2.1 charset guard (single segment, no `..`/
  separators → `invalid_name`) + canonicalize-and-contain within
  `<repo_root>/.pohunek/` (rejects symlink escape) + a read surface limited to
  `prompts/*.tmpl` and the two TOMLs. Applies identically to the in-repo `prompt=`
  field and the wire/CLI `<name>`.
- **A hook wedging `session.new`.** Hooks reuse the *bounded* setup-script discipline,
  unlike `WorktreeManager`'s own unbounded git executor (`worktree/mod.rs:957`), so a
  hung hook cannot wedge create.
- **`agent-state` hooks stalling the audit log.** Separate broadcast subscriber task,
  last-fired tracking + debounce; never blocks the drain.
- **Extra daemon round-trip per launch.** `project.action` adds one request before
  `session new`. Cheap (the daemon is already up for `session new`) and the cost of
  having one source of truth.

## Security & trust

- **No trust gate (design decision).** The daemon honors in-repo `.pohunek/`
  definitions and hooks **without prompting**, as `.pohunek/setup` does today. Single
  operator + "I clone my own repos". **Residual risk, stated plainly:** cloning an
  untrusted repo and starting a session on it runs that repo's hooks **as the daemon
  user** on the daemon's host, and lets that repo's templates pick a built-in agent.
- **In-repo definitions cannot set what the daemon execs beyond the A.5 safe subset.**
  No arbitrary `program`/`argv`/`env` from a repo; `agent` is a **name** resolving to a
  compiled base kind or a host-authored profile (Part C). `program`/`argv`/`env` live
  only in those host/compiled sources, never in a repo or the wire — and a host
  profile dir is owner-only permission-gated (C.5).
- **Daemon-side file read keyed on a repo/wire-controlled name (the pivot's net-new
  channel).** The daemon reads files whose *names* come from an untrusted repo (a
  template's `prompt=`) or the wire (`project prompt <name>`) and returns their
  *contents* over the protocol — a channel the client-side launcher never had. It is
  bounded by the A.2.1 guards: (1) single-segment charset guard against `..`/path
  separators, (2) canonicalize + containment within `<repo_root>/.pohunek/` so a
  committed symlink cannot escape to `/etc/shadow` or `~/.ssh/`, (3) the read surface
  is only `.pohunek/{templates,actions}.toml` + `prompts/*.tmpl`. This extends the
  "no path on the wire / safe id" model (projects.md Decision 1) from ids to names.
- **Hooks run with a cleared environment (primary secret control).** Every hook is
  spawned `.env_clear()`-ed with an explicit allowlist (`PATH`, `HOME`, `POHUNEK_*`).
  A **new requirement** — `run_setup_script` today leaks the full daemon env. The
  `POHUNEK_*` context carries only ids/paths/branch/activity/stop-reason — never tokens.
- **No secret reaches the event log / wire from hooks.** Hook stdout/stderr →
  `/dev/null`; the event log and `SessionWarning` carry only structured metadata.
- **The metadata store keeps its no-secrets invariant under Part C.** The resume
  snapshot persists only the **non-secret structural** profile fields; profile `env`
  (the one field that could carry a token) is **never written to the store** —
  re-resolved at resume instead (C.4). Profile `env` is non-secret config by contract;
  secrets reach the agent via the daemon's inherited environment as today, not via a
  profile or the store.
- **Returned prompt content is host-side config text, not arbitrary host files.** For
  the **host** layer it is the operator's own template; for the **in-repo** layer it
  is repo-controlled (not "operator's own"), so the safety rests entirely on the
  A.2.1 containment + charset guards — without them `project.prompt` would be an
  arbitrary-file read. Rendering with provider data happens caller-side.
- **Agent profiles (Part C) are host-authored and owner-only; the wire/repo only
  names them.** `program`/`argv`/`env` come from `~/.config/pohunek/agents/` on the
  daemon host (owner-only permission-gated, skipped + warned otherwise) or a compiled
  base kind — never from the wire or a repo. The free-string `agent` is a **name**
  (A.2.1 charset guard, fail-closed `agent_profile_not_found`), so `resolve_binary`
  still gates the program over the operator's pre-approved set; a profile body is
  **never** accepted over the wire (that would re-open A.5).
- **Argv-injection boundaries unchanged.** Branch/base validation
  (`validate_git_ref_arg`, `worktree/mod.rs:594`) and `--end-of-options` git guards
  are untouched; hooks receive context via env, not git argv. A profile's resume
  `ref_kind` still routes through the `SessionRef` ctor that rejects leading-dash
  values (`agent/mod.rs:100`).
- **Rendering trust boundary preserved.** Provider-controlled values still flow
  through the single-pass renderer that refuses to re-expand `${...}` and rejects
  unknown variables (`lib.sh:106`); rofi rows stay tab-flattened
  (`pohunek-rofi-issue:65`). The browser must apply the same renderer contract.

## No backward compatibility (experimental project)

Same stance as the Projects work ([`projects-plan.md`](projects-plan.md)) and
[`NEXT.md`](../../NEXT.md): **backward compatibility is an explicit non-goal.** This
RFC contains genuinely breaking changes; none of them carry a compat shim.

- **CLI and daemon are assumed to be the same build.** We never handle an old CLI
  talking to a new daemon (or vice versa) — no `method_not_found` "please upgrade"
  translation, no version-negotiation shims. The operator upgrades all hosts' daemons
  together (already "a session-killing event by design", `architecture.md`).
- **Wire shapes change freely.** Concretely in this RFC: `agent` flips from the
  `AgentKind` unit enum to a **free string** (Part C) across `SessionNewParams`,
  `SessionInfo`, `SessionListFilter`, `SessionReportNativeIdParams`, and
  `HostCapabilities`; the new `project.actions`/`project.action`/`project.prompt`
  methods and the `SessionWarningKind::Hook` variant are added without a
  `PROTOCOL_VERSION` bump (additive policy, `protocol/src/version.rs`). The protocol's
  pre-1.0 roundtrip tests (`crates/protocol/tests/roundtrip.rs:59`) are **rewritten**
  to the new shapes, not kept dual-form.
- **On-disk state may be wiped on upgrade.** `ResumeBinding` gains the agent name + the
  C.4 **structural** snapshot (not the profile `env`); we write **no migration** — a
  pre-existing store that lacks them is discarded, not upgraded. The metadata store
  carries no compat guarantee (and, per C.4/§Storage, no secrets).
- **Config formats change freely pre-1.0.** The daemon-side definition formats are
  **TOML** (`.pohunek/{templates,actions}.toml` + host equivalents, read by the Rust
  `toml` crate — no Python); the in-repo `.pohunek/` schema, the agent-profile TOML,
  and the `${var}` prompt contract may all change shape. The **client** `launcher.conf`
  **stays flat `key=value`** (its parser is unchanged) but **shrinks**: the per-launch
  keys now resolved daemon-side (`agent`/`project`/prompt selection) move out, leaving
  only host-only keys (`terminal`, `rofi_bin`, `linear_cli`, `list_timeout_seconds`,
  …). The operator re-runs `pohunek setup` rather than relying on a migrator; removed
  keys are **not** kept valid.
- **The public stability promise stays deferred** until the protocol settles in daily
  use (per `NEXT.md`); these surfaces are explicitly unstable until then.

## Out of scope

- Mutating/vetoing hooks (return values that change git args, paths, env, or abort).
  v1 is strictly react-only.
- Daemon-side **rendering** of prompts / a `session new --action` that fetches provider
  data — blocked on providers being daemon-reachable (Phase 4 Slice E). v1 keeps
  fetch+render caller-side.
- Arbitrary `program`/`argv`/`env` from a **repo/template or the wire** (the unsafe
  superset of A.5) — these live only in a host agent profile (Part C).
- Shipping an agent profile **definition** over the wire (re-opens A.5); profiles are
  host-authored only. Profile **inheritance graphs** / per-profile permission policies
  (à la Kandev) are also out — a profile extends exactly one base kind.
- Per-project **detection manifest** overrides from a repo (`detect/manifests/*.toml`);
  a *host* profile may override its manifest (C.3), a repo may not.
- A trust/approval gate or hash-pinning for repo-supplied code.
- New provider adapters; `provider` stays `linear_issue`/`github_pr`/`none`.
- Storing definitions/profiles in the metadata store (only the resume snapshot rides
  there); a `worktree.*` RPC method.

## Decisions (resolved)

1. **`template` / `action` / `prompt`.** An *action* is a named launchable operation;
   a *template* is the recipe (`{agent, base_branch, prompt, branch rule}`) it runs; a
   *prompt* is a named instruction template, one field of a template. Actions
   reference templates; templates reference a prompt by name.
2. **`actions` means the rofi/browser launch flow** ("process this issue/PR"), not the
   CLI subcommand enums and not the lifecycle hooks.
3. **Source of truth = the daemon.** Templates/actions/prompts are resolved by the
   daemon (`project.*` methods) so the rofi launcher and the Phase 4 browser read
   identical definitions, and **remote** projects work. (This revises the earlier
   "client config only" direction — the daemon being co-located with every repo is
   what makes per-repo overrides and the prompt chain work for remote.)
4. **Config layering = in-repo `.pohunek/` over host `~/.config/pohunek/`**, resolved
   daemon-side. The per-project home is the **repo** (`<repo_root>/.pohunek/`), not an
   id-keyed file; an additional host-side id-keyed override file is **deferred** to
   Open question 3. Config format is **TOML** for definitions; prompts are `.tmpl`
   files with the documented `${var}` contract; all names are validated single
   segments (A.2.1).
5. **No trust gate** — same posture as `.pohunek/setup`; bounded by the A.5 safe subset
   (repo can't choose what binary/argv/env the daemon execs) and hook env-clear.
6. **Prompt resolution is fail-closed:** in-repo prompt → host default → **error**
   (`prompt_not_found`); no silent fallback.
7. **Hooks are react-only** (no veto in v1), reuse the env-cleared setup-script
   discipline, fire across worktree create/remove + session start/stop + agent state
   change, are filesystem-discovered, and are symmetric with Part A's discovery/
   layering. `session-stop` is decoupled from worktree removal (a stopped session keeps
   its worktree) and fires on all terminal reasons with `POHUNEK_STOP_REASON`.
8. **Agent profiles extend, not replace.** The `shell`/`codex`/`claude` **base kinds
   stay compiled-in** (with their trusted manifests/resume); a host profile names a
   base and overrides program/args/env/input-rules/resume/manifest. No profile → bare
   base kind behaves exactly as today. A **`ShellAdapter` is introduced** so all three
   base kinds resolve through one adapter path (Shell's `ShellCommand` special-case is
   folded in); `base = "shell"` forces `resumable = false`.
9. **A profile inherits its base kind's manifest + resume by default**, overriding only
   what it declares; an overridden manifest is loaded via the capped, non-panicking
   `Manifest::parse_str`.
10. **`agent` is a free-string name on the wire**, resolved daemon-side on the target
    host (charset-guarded → host profile → base kind → fail-closed
    `agent_profile_not_found`). `AgentKind` survives only as the internal base-kind
    type. `session list --filter agent=<base>` **groups by base kind** (matches the
    name or the resolved profile's base). Launch-affecting profile fields are
    **snapshotted onto the session entry** (copied verbatim on every re-persist, never
    re-resolved) so a later profile edit/delete cannot break an in-flight resume
    (Kandev pattern). The whole `agents/` tree is owner-only + containment-guarded; a
    profile body is never sent over the wire.

## Open questions

1. **Daemon-side rendering / `session new --action`.** Once providers are daemon- or
   backend-reachable (Phase 4 Slice E), should the daemon render prompts and accept
   `session new --action <name>` directly, instead of the caller rendering? Part A is
   shaped so this is additive.
2. **Dynamic action menu in rofi.** `project.actions` could drive a rofi menu of all a
   project's actions (instead of fixed `$mod+i`/`$mod+p` keybinds). Nice-to-have.
3. **Per-project file vs sections** for any host-side per-project overrides
   (`projects/<id>.toml` vs `[project.<id>]` sections). Both viable.
4. **Opt-in blocking hooks.** Should a future `pre-create`/`pre-remove` veto via a
   documented exit code + error class? Out of scope for v1; `worktree/mod.rs:256` is
   the seam.
5. **`<event>.d/` directories** (multiple ordered scripts per event) — single-file
   form first.
6. **`post-create` seam placement** — before vs after the binding persist (`:284`),
   trading the rollback-leak window against firing before ownership is recorded.
7. **Profile `program` as PATH-name vs absolute path** (Part C) — allow both, or
   require absolute for a profile that overrides the program, to make the exec target
   unambiguous on a remote host?
8. **A genuinely new agent with no fitting base kind** (Part C) — v1 requires every
   profile to declare one of the three base kinds. A future fourth base kind (or a
   `base = "none"` fully self-contained profile) is the escape hatch if a new agent's
   resume/detection doesn't resemble claude/codex/shell.
9. **`pohunek agent` CLI surface** — should there be `pohunek agent list`/`show` to
   inspect resolved profiles on a host (mirrors `project prompt`)? Likely yes; deferred
   to the plan.

## Cross-references

- [`projects.md`](projects.md) (Decision 1 "no path on the wire"; Decision 3 "a
  per-project/global override is a possible later addition") — the override precedent.
- [`projects-followups.md`](projects-followups.md) — "per-project config/policy beyond
  `default_base_branch`" was out of scope; this RFC takes it up.
- [`architecture.md`](../architecture.md) — "Projects and Worktree Isolation";
  "Configuration, State, and Log Storage". Reconcile on landing.
- [`../phases/05-rofi-sway-launcher.md`](../phases/05-rofi-sway-launcher.md) — prompt
  templates + launcher actions.
- [`../phases/04-browser-control-center.md`](../phases/04-browser-control-center.md) —
  "one source of truth, two clients"; this RFC makes the daemon that source.
- [`NEXT.md`](../../NEXT.md) — experimental / no-backward-compatibility stance.
