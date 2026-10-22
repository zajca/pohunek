# Pohunek — Roadmap

Consolidated, top-level roadmap for the whole project: a reconciled snapshot
and index, not a planning authority.

**Source of truth.** Delivery scope, sequencing, and status live in GitHub:

- the epic issues and their native sub-issue and blocked-by links;
- the [repository milestones](https://github.com/zajca/pohunek/milestones),
  including the agent program milestones A–E and
  [Complete macOS support](https://github.com/zajca/pohunek/milestone/2);
- the GitHub Projects of `zajca`: **Pohunek** (project 1, the default board),
  **Team control plane** (2), **Pohunek test performance** (3), **Pohunek macOS
  support** (4), **Runtime plugin packages** (5), and **Pohunek dev workflow
  performance II** (7).

The Projects are private, so public contributors work from the issues and
milestones, which carry the same scope and dependencies. Where this document
and GitHub disagree, GitHub wins and this document is reconciled again. Status
here comes from issue state and sub-issue counts, not from Project board
columns. The per-phase documents under [`docs/phases/`](phases/) and
[`docs/design/`](design/) record product and architecture rationale.

This repository is the core: daemon, session worker, CLI, relay, Rust crates,
and the TypeScript SDK workspace. It ships no UI. The native desktop app, the
mesh-local browser control center, the rofi/sway launchers, and issue/PR
providers live in [`zajca/pohunek-work`](https://github.com/zajca/pohunek-work)
and consume core only through public contracts.

Last reconciled: 2026-10-14. A section that names an earlier date was last checked on that date.

---

## 1. Shipped foundations

| Track | What it delivers | Where |
|---|---|---|
| **Phase 1 — Core local sessions** | Daemon owns PTYs/processes; CLI new/list/inspect/attach/detach/stop; state engine; resume via native session IDs; worktree-per-session; file-based stores. | [`phases/01`](phases/01-core-local-sessions.md), `crates/daemon`, `crates/cli` |
| **Milestone 5 — State engine** | Debounced `agent_state` signal (`working`/`blocked`/`idle`) with recorded source, in `session.inspect` + event stream. | [`...milestone-5.md`](superpowers/plans/2026-06-17-state-engine-milestone-5.md) |
| **Phase 2 — Remote over NetBird** | Tokenless discovery; `host list/discover/inspect`; full remote session lifecycle over the mesh; daemon binds NetBird-only. | [`phases/02`](phases/02-remote-netbird.md) |
| **Milestone 10** | `--json` everywhere + `cli_usage` typed error envelope. | branch `milestone-10` (merged) |
| **Projects** | Git-repo awareness; `project` commands; all 7 follow-ups (F1–F7) resolved. | [`design/projects*.md`](design/) |
| **Per-project actions + worktree hooks + agent profiles** | All slices incl. session/agent-state hooks (B2, `ed6b778`); env-clear + allowlist; host agent profiles. | [`design/per-project-actions-*`](design/) |
| **Phase 5 / Milestone 12 — launcher chassis** | `session list --filter`; `session new --input` (atomic launch). The rofi/sway launcher scripts live in `zajca/pohunek-work`. | [`phases/05`](phases/05-rofi-sway-launcher.md) |
| **Universal Assistant (P0–P11)** | `pohunek assistant`, the embedded knowledge bundle, the hook write gate, `cargo xtask eval`, and `cargo xtask docs site` with offline docs in each release tarball. | [`design/universal-assistant-plan.md`](design/universal-assistant-plan.md) |
| **Track S — Public API + SDKs** | S.1 Rust SDK (`crates/client`); S.2 versioned public API with version negotiation ([`public-api.md`](public-api.md)); S.3 TypeScript SDK (`sdk/ts/`) with `ts-rs` generated types, a CI drift check, and a WebSocket transport tested against the loopback relay in `@pohunek/testkit`. | `crates/client`, `sdk/ts/` |
| **Track H — Hermes runtime and operator plugin** | Local Hermes Agent runtime (verified on `0.20.0`, accepted up to the next major) in Pohunek-owned PTYs, exact native resume, and the operator plugin for a selected Hermes profile with an external access policy. Both are served only by the official `pohunek.runtime.hermes` package (#147, Linux x86_64; Apple Silicon archive since #929). | [operator guide](knowledge/guides/hermes-operator.md), [runbook](runbooks/hermes-operator-plugin.md), [migration](migrations/hermes-operator-plugin.md) |
| **Track B / Track D — browser control center and native desktop app** | Browser M1 (multi-host sessions workspace, notifications inbox, in-browser terminal) and the Iced desktop control plane are clients maintained in `zajca/pohunek-work`. Core keeps the shared contracts: the protocol, both SDKs, `crates/prompt` (`pohunek prompt render`), opaque `link.*` session metadata, and `session new --input`. The browser M2/M3 plan to grow into a public multi-user backend is superseded by the [team-relay RFC](design/team-relay-control-plane-rfc.md). | [`phases/04`](phases/04-browser-control-center.md), [Track B plan](design/track-b-web-control-center-plan-2026-07-22.md), [Track D design](design/track-d-native-app.md) |
| **Test performance milestone** | Milestone "Test performance" (13/13 closed) and project 3 (13/13 Done). | [milestone 1](https://github.com/zajca/pohunek/milestone/1) |

**Public API stability.** There is no compatibility promise before 1.0. SDK
semver tracks the protocol version, and breaking changes are allowed with a
version bump until the promise is made. Clients pin the core release and the
`PROTOCOL_VERSION` they were built against; core keeps no compatibility shim
for them.

---

## 2. Open epics and tracks

Counts are closed/total native sub-issues on 2026-10-12.

### Agent program — [#797](https://github.com/zajca/pohunek/issues/797) (milestones A–E)

Makes the repository usable by agents, acting for their owners, to discover
Pohunek, pick bounded work, coordinate, and contribute. #797 has 4/22 direct
children closed. The five outcome milestones reuse the existing epics below
instead of competing with them.

| Milestone | Status on 2026-10-12 |
|---|---|
| [A. Agents discover Pohunek and make their first contribution](https://github.com/zajca/pohunek/milestone/4) | 3 closed, 6 open. Contributor authority [#798](https://github.com/zajca/pohunek/issues/798), entry points and contribution skill [#799](https://github.com/zajca/pohunek/issues/799), and issue contracts [#800](https://github.com/zajca/pohunek/issues/800) closed 2026-10-12. Toolchain pin [#360](https://github.com/zajca/pohunek/issues/360) is in progress (PR #850 merged; issue open). Open: [#801](https://github.com/zajca/pohunek/issues/801)–[#804](https://github.com/zajca/pohunek/issues/804) and this roadmap ([#517](https://github.com/zajca/pohunek/issues/517)). |
| [B. Agents from different owners coordinate work safely](https://github.com/zajca/pohunek/milestone/5) | 0 closed, 9 open: claim service [#805](https://github.com/zajca/pohunek/issues/805)–[#811](https://github.com/zajca/pohunek/issues/811), [#817](https://github.com/zajca/pohunek/issues/817), [#818](https://github.com/zajca/pohunek/issues/818). #805 (service contract) is in progress; the service itself is built in `zajca/pohunek-work`. |
| [C. Agents operate and extend Pohunek reliably](https://github.com/zajca/pohunek/milestone/6) | 2 closed ([#812](https://github.com/zajca/pohunek/issues/812), [#480](https://github.com/zajca/pohunek/issues/480)), 22 open: the runtime-package, workflow-plugin, and Pi epics below, doctor checks [#68](https://github.com/zajca/pohunek/issues/68) (in progress), operating recipes [#813](https://github.com/zajca/pohunek/issues/813), external runtime author [#814](https://github.com/zajca/pohunek/issues/814), and the assistant roles design [#823](https://github.com/zajca/pohunek/issues/823). |
| [D. Agents delegate work with verifiable results](https://github.com/zajca/pohunek/milestone/7) | 0 closed, 15 open: delegated task runs (#182, WS1–WS8). |
| [E. Pohunek develops itself through accountable agents](https://github.com/zajca/pohunek/milestone/8) | 0 closed, 11 open: dark factory (#185, FS0–FS8), [#815](https://github.com/zajca/pohunek/issues/815), [#816](https://github.com/zajca/pohunek/issues/816). |

### Runtime plugin packages — [#139](https://github.com/zajca/pohunek/issues/139) (milestone C, project 5)

Runtimes become signed, content-addressed packages installed per host; only
`shell` stays built in. **37/46** on 2026-10-14. Landed: the accepted RFC (#152), the
`RuntimeId` protocol and generic runtime host (#140), the authenticated package
registry (#141), the CLI lifecycle and profile migration (#143), generic hook
schemas and integration activation (#144), digest-bound package releases with
catalog signing (#150), the official Pi package (#538), the official Codex
(#145), Claude Code (#146) and Hermes (#147) packages with their compiled-in
runtimes removed, the package CI and compatibility matrix (#149), workflow
plugins (#148), the package documentation (#151), and the decision that core
consumes no plugin data in v1 (#480). Open:

- Follow-ups #868, #870, #899, #911 and #1003.
- One-step host setup and upgrade to the official packages (#1011, #1013).
- Workflow plugin follow-ups: macOS child cleanup after a `SIGKILL`ed
  trampoline (#1019), and reactions in the install review and the service
  uninstall refusal (#1061).

Guide: [`knowledge/guides/runtime-packages.md`](knowledge/guides/runtime-packages.md).

### Workflow plugins — [#148](https://github.com/zajca/pohunek/issues/148) (child of #139; milestone C, project 5)

Bounded out-of-process plugins with declared actions and jobs. **4/4** on 2026-10-14. Landed:
`session.list` metadata filter (#327) and the display decision (#328: core adds
no UI data contract; `zajca/pohunek-work` renders plugin data from public
contracts). Also landed under #148: the CLI-owned workflow plugin store,
the strict `workflow.toml` manifest and the consent install (`pohunek
workflow`), and CLI action invocation with `--json` results and shell
completion ([#325](https://github.com/zajca/pohunek/issues/325)), periodic
jobs and long-running services as user service manager units with `pohunek
workflow status` ([#326](https://github.com/zajca/pohunek/issues/326)), and
event reactions through one dispatcher unit per namespace (#148). Guide:
[`knowledge/guides/workflow-plugins.md`](knowledge/guides/workflow-plugins.md).
Core consumes no plugin data for launch-profile policy in v1
([#480](https://github.com/zajca/pohunek/issues/480), closed).

### Pi coding agent — [#50](https://github.com/zajca/pohunek/issues/50) (milestone C)

**1/5** (#51 generalized resume/fork launch specs closed). Pi ships as the
official runtime package (#538, guide
[`pi-package.md`](knowledge/guides/pi-package.md)). The #812 reconciliation on
2026-10-11 keeps #50 open for durable-worker, activity/turn-completion,
security, and external-client acceptance; #52–#54 are not to be implemented as
the original compiled-runtime chain, and #55 must restate the end-to-end matrix
against the installed package. #546 (resumable forks with an assigned
reference, Pi package 1.0.1) and #547 (blocked dialogs, overlay-aware idle and
session-dir overrides, Pi package 1.0.2) are delivered.

### Delegated task runs — [#182](https://github.com/zajca/pohunek/issues/182) (milestone D)

Tasks and turns over ordinary sessions: causal settlement, `task.wait`, typed
results with evidence and checks, an optional MCP adapter.
[RFC](design/delegated-task-runs-rfc.md). **2/16.** The RFCs are on `main`
(#218) and amended (#510). Implementation has started: WS1 protocol
[#219](https://github.com/zajca/pohunek/issues/219) is in progress; WS2–WS8
([#220](https://github.com/zajca/pohunek/issues/220)–[#232](https://github.com/zajca/pohunek/issues/232))
are open and follow the chain in section 3.

### Relay dark factory — [#185](https://github.com/zajca/pohunek/issues/185) (milestone E)

Unattended manager/auditor delegation over the team relay, built as a relay
client. [RFC](design/relay-dark-factory-rfc.md). **0/8.** FS0
[#233](https://github.com/zajca/pohunek/issues/233) (amend the team relay RFC
for `task.*`) is in progress; FS2–FS8
([#234](https://github.com/zajca/pohunek/issues/234)–[#240](https://github.com/zajca/pohunek/issues/240))
depend on both the task-run workstreams and the relay implementation. Factory
client work items live in `zajca/pohunek-work`.

### Session durability and recovery — [#426](https://github.com/zajca/pohunek/issues/426) (project 1)

Clean recovery from a host-wide session loss. **2/6.** Landed in the 0.34
milestone: a dead worker is classified `lost` (#420) and recovery binds the last
conversation (#421). Open: loss cause and one crashed state
[#422](https://github.com/zajca/pohunek/issues/422), one-action in-place
recovery [#423](https://github.com/zajca/pohunek/issues/423), a crash note for
the recovered agent [#424](https://github.com/zajca/pohunek/issues/424), and
host memory warnings and per-session bounds
[#425](https://github.com/zajca/pohunek/issues/425). Related:
[#402](https://github.com/zajca/pohunek/issues/402) (stop with force-closed PTY
output leaves a dead root; in progress, PR #444 open).

### Activity detection — no epic

[#59](https://github.com/zajca/pohunek/issues/59) explain diagnostics,
[#60](https://github.com/zajca/pohunek/issues/60) refreshed Codex/Claude rules,
[#61](https://github.com/zajca/pohunek/issues/61) manifest viewer and
visible-state semantics, [#63](https://github.com/zajca/pohunek/issues/63)
manifest reload and status, and
[#67](https://github.com/zajca/pohunek/issues/67) state arbitration are open
with no parent epic, no milestone, and no Project. Runtime-specific detection
gaps are tracked under the package epics (#592, #613, #547).

### Test stability and feedback loop — [#359](https://github.com/zajca/pohunek/issues/359) (project 1)

Test and CI stability by failure class. **7/21.** Landed: deterministic timing
(P2 #362), hermetic `TestEnv` (P3 #363), Linux-side macOS clippy (P5 #365),
optimized test hot-loop dependencies (P7 #641). Open: CI tiers P6
[#477](https://github.com/zajca/pohunek/issues/477), toolchain pin P0
[#360](https://github.com/zajca/pohunek/issues/360) (in progress, pin merged in
PR #850, also in milestone A), macOS nextest sharding P1
[#361](https://github.com/zajca/pohunek/issues/361) (3/4, open #377), flake
detection P4 [#364](https://github.com/zajca/pohunek/issues/364), and the
cost/maintenance items P8–P14
([#642](https://github.com/zajca/pohunek/issues/642)–[#648](https://github.com/zajca/pohunek/issues/648)),
#774, #847, #855. The unit-test migration of the testing policy is
[#671](https://github.com/zajca/pohunek/issues/671) (in progress). Project 7
(dev workflow performance II) has 21 of 23 issues closed; #272 and #323 remain
open.

### macOS host and client support — [#94](https://github.com/zajca/pohunek/issues/94) ([milestone](https://github.com/zajca/pohunek/milestone/2), project 4)

Native Apple Silicon hosts, macOS 14.0 minimum target tested on macOS 15
(#1224), launchd-owned durable workers ([RFC](design/macos-support-rfc.md)); Intel Macs are out of scope.
**Supported** on Apple Silicon (#1464); the milestone has 6 open and 6 closed issues. Landed: platform
contracts (#95), runtime paths (#96), process inspection (#97), peer identity
(#98), portable PTY I/O (#99), external observation and integrations (#101), and
ad-hoc signed archives with a Homebrew tap (#481). Open:

- launchd supervision [#100](https://github.com/zajca/pohunek/issues/100) —
  5/9; implementation is on `main`, the manual logout/login and reboot lifetime
  evidence is still missing (open: #186, #187, #189, #194).
- CLI setup and diagnostics [#102](https://github.com/zajca/pohunek/issues/102)
  — 1/4 (open: #319, #332, #333).
- owner WebUI and direct-overlay workflows
  [#103](https://github.com/zajca/pohunek/issues/103) — its only sub-issue is
  closed; the issue is open.
- installation and release artifacts
  [#104](https://github.com/zajca/pohunek/issues/104) — rescoped to ad-hoc
  signing; real-Mac install/upgrade/uninstall acceptance remains.
- follow-up [#105](https://github.com/zajca/pohunek/issues/105) — native
  durability, security and cross-stack acceptance; #355 is closed.

The core part covers CLI, daemon, worker, SDK, and archives; the macOS GUI and
owner web control center are tracked in `zajca/pohunek-work`.

### Optional team relay — [#56](https://github.com/zajca/pohunek/issues/56) (project 2)

One trusted, public, PostgreSQL-backed `pohunek-relayd`; hosts initiate an
embedded userspace WireGuard tunnel; the relay adds principals, teams, session
ACLs, routing, aggregation, audit, and quotas. Standalone and direct NetBird
operation stay first-class. [RFC](design/team-relay-control-plane-rfc.md).
**4/14.** Landed: overlay abstraction (#69), architecture and threat model
(#80), host identity and single-relay state (#81), relay foundation and
authentication (#85); outside the sub-issue tree, RFC reconciliation (#91),
Google/GitHub admission (#92), and account linking (#107). Open: enrollment and
WireGuard transport [#72](https://github.com/zajca/pohunek/issues/72) (all
native blockers closed), the v4 host link
[#70](https://github.com/zajca/pohunek/issues/70), HostShare
[#82](https://github.com/zajca/pohunek/issues/82), visibility and sharing
[#83](https://github.com/zajca/pohunek/issues/83), snapshots
[#84](https://github.com/zajca/pohunek/issues/84), routing core
[#71](https://github.com/zajca/pohunek/issues/71), team surfaces
[#86](https://github.com/zajca/pohunek/issues/86), operational hardening
[#87](https://github.com/zajca/pohunek/issues/87), and post-release
[#73](https://github.com/zajca/pohunek/issues/73) and
[#88](https://github.com/zajca/pohunek/issues/88). Team administration API
[#108](https://github.com/zajca/pohunek/issues/108) is open outside the epic and
outside project 2. The relay stays in core for now and later moves, with the
team web UI of #86, into a dedicated repository (owner decision on #415,
2026-10-02); the trigger and cross-repository reuse are open in
[#417](https://github.com/zajca/pohunek/issues/417).

### UI move to `zajca/pohunek-work` — [#415](https://github.com/zajca/pohunek/issues/415)

The core side is delivered: its three sub-issues are closed and core release
v0.32.1 (2026-10-03) was published without GUI or web assets, followed by
v0.32.3, the release `zajca/pohunek-work` pins. #415 stays open for the receiving side, tracked in
[pohunek-work#10](https://github.com/zajca/pohunek-work/issues/10) and
[pohunek-work#33](https://github.com/zajca/pohunek-work/issues/33).

---

## 3. Recommended sequence

Derived from native blocked-by links on 2026-10-12. `A → B` means A blocks B.
Edges marked *(text)* exist only in issue text, not as native links. Lanes run
in parallel unless a cross-lane edge says otherwise.

1. **Contribution loop (A, B).** #477 → #360 → #801. #700 → #802 → #803 (also
   after closed #800) → #804 (also after #801). B: #805 → #806 → #807 and #808
   (#808 also after #802);
   #808 and #807 → #809; #802 and #808 → #817; #805 → #818; #807, #808, #809,
   #817, #818 → #810 → #811 (also after #803).
2. **Runtime packages and plugins (C).** #145, #146, #147 → #149; #145, #146,
   #147, #148 → #151; #149, #151, #804 → #814. #68 → #813. #325 and #326 are
   sub-issues of #148, not native blockers of it *(text: "#148 → #325/#326" in
   #797)*. Pi: the native edge #54 → #55 exists, but the #812 reconciliation on
   #50 supersedes the old #52 → #53 → #54 chain; the remaining Pi order needs a
   decision on #50.
3. **Delegated task runs (D).** #219 → #220 → #221 → #222 → #223 and #224;
   #224 → #225 → #226 (also after #222); #222 → #227 and #228; #223 and #226 →
   #229; #226 → #230; #229 → #231; #227, #228, #229, #230 → #232.
4. **Team relay.** #72 → #70 → #82 and #83; #82 → #84; #70, #72, #82, #83, #84
   → #71 → #86 and #87 (#87 also after #72) → #56 closes. Post-release: #71,
   #84, #86, #87 → #73; #82, #86, #87 → #88. #108 has no native blockers and
   blocks #238. #417 is a decision with no native edges *(text: its trigger is an
   open question, for example after #86 or the first relay release)*.
5. **Dark factory (E).** FS0 #233 is unblocked. #233, #221, #225, #82 → #234;
   #219, #233, #234, #70, #71, #83 → #235; #235, #70, #87 → #236; #235, #226,
   #84 → #237; #235, #237, #108 → #238; #229, #230, #235–#238 → #239; #232,
   #239 → #240. Then #239, #804, #811 → #815; #814, #240, #815 → #816. E
   therefore follows D and the relay lane.
6. **Platform and reliability.** macOS: #100, #102, #103, #104 → #105;
   #100–#103 → #104 *(text: #104 "Dependencies")*. Durability: #422 → #423 and
   #424; #425 is independent. Tests: #477 → #360 → #361 → #364.

#360's pin landed through PR #850 while its native blocker #477 is still
open; the link stays authoritative until it is changed on the issue.

Shortest path to a first public contribution: #477 → #360 → #801, with #700 →
#802 → #803, closing on #804. The longest chain runs D (#219 … #232) and the
relay (#72 … #87) into E (#233 … #240) and #816.

---

## 4. Deferred / out of scope

- **Application auth in the mesh-local owner backend (external client)** —
  intentionally absent under the owner-path NetBird/filesystem trust boundary.
  The relay foundation provides generic OIDC, credential lifecycle, and account
  linking; team administration and session ACLs remain
  [#108](https://github.com/zajca/pohunek/issues/108) and
  [#83](https://github.com/zajca/pohunek/issues/83).
- **In-tree provider adapters in the chassis** — never; providers stay shell-out
  (`gh`) / GraphQL (Linear) in the clients.
- **Any UI in core** — the native desktop app, browser control center, and
  launchers live in `zajca/pohunek-work`; libghostty / GTK / Electron GUIs are
  dropped.
- **Filesystem scanning / auto-discovery of repos; cross-host project unification;
  GC of stale auto-projects; per-project policy beyond `default_base_branch`** —
  out of scope (from the Projects design).
- **Intel Macs and `x86_64-apple-darwin` artifacts** — outside the macOS
  release scope (#94).
