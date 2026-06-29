# Plan: Generic per-session metadata

Date: 2026-06-27
Status: proposed (awaiting implementation)

## Goal

Allow a pohunek session to carry an arbitrary, caller-defined `metadata` map
(e.g. a GitHub PR number, a Linear issue id, a ticket URL, a free-form label).
Today nothing of the sort is stored: a session only persists structural fields
(`branch`, `project_id`, resume snapshot, …) and the only place PR/issue context
could hide is the opaque `branch` string. This adds metadata as a first-class,
queryable, persisted field.

### Design decisions (confirmed)

1. **Shape**: `BTreeMap<String, String>` — deterministic serialization,
   filterable (`session list --filter meta.<key>=<value>` later), display-friendly.
   Structured values can be stored as a JSON string by the caller. The protocol
   crate does **not** take a `serde_json::Value` dependency in its public API.
2. **Write paths**: settable at `session.new` (new `metadata` param) **and** via a
   new mutation RPC `session.set_metadata` with **merge** semantics.
3. **Lifetime**: matches the resume binding — lives with a resumable session,
   survives a daemon restart, is dropped on `stop`. No new store record kind.

### Merge semantics for `session.set_metadata`

Param map is `BTreeMap<String, Option<String>>`:
- `Some(value)` — insert or overwrite the key.
- `None` (JSON `null`) — delete the key if present.
- Keys not mentioned are left untouched.

This gives set + delete + partial-update in one round-trip without a separate
"replace" flag.

## Definition of done

- `session.new` accepts a `metadata` map; it appears on the returned `SessionInfo`.
- `session.set_metadata` merges per the semantics above and returns the updated
  `SessionInfo`; unknown session id returns the standard not-found error.
- Metadata is persisted in `ResumeBinding` and restored on daemon restart for a
  resumable session.
- Size/shape limits are enforced and exceeding them is a clear, fail-fast error
  (no silent truncation).
- Store doc-comment still truthfully states metadata is **not** for secrets.
- `xtask` protocol descriptor + generated docs include the new method.
- Tests: new-with-metadata, set merge (insert/overwrite/delete), limit rejection,
  persistence round-trip across a simulated restart.

## Constraints / guardrails

- **Secrets**: the store self-describes as "No secrets are ever written"
  (`crates/daemon/src/store/mod.rs:26`). Free-form metadata can break that, so:
  - update that doc-comment to note metadata is owner-controlled and **must not**
    hold secrets;
  - enforce hard limits (below) to bound blast radius.
- **No hardcoded limits in code bodies**: define the limits as named constants in
  one place (mirror where other daemon limits live — verify the existing pattern
  before adding; if there is a daemon config/limits module, put them there).
  Suggested defaults (adjust to match existing conventions):
  - max keys per session: 32
  - max key length: 64 bytes
  - max value length: 4096 bytes
  - max total serialized metadata: 16 KiB
- **No back-compat gymnastics**: pohunek is experimental and the store may be
  wiped on upgrade, but still add `#[serde(default)]` so an in-place old store
  line loads cleanly rather than failing the whole file read.
- Validation lives in the daemon registry method (single choke point used by both
  `session.new` and `session.set_metadata`), not in the protocol structs.

## Insertion points (file:line)

### Protocol crate (`crates/protocol`)

1. **Method constant** — `crates/protocol/src/lib.rs:~98`
   Add after `SESSION_REPORT_NATIVE_ID`:
   ```rust
   pub const SESSION_SET_METADATA: &str = "session.set_metadata";
   ```

2. **`SessionNewParams`** — `crates/protocol/src/session.rs:51-89`
   Add `use std::collections::BTreeMap;` at top, then after `input` (line 88):
   ```rust
   /// Per-session metadata as a flat map, set at creation. Owner-controlled;
   /// must not contain secrets.
   #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
   pub metadata: BTreeMap<String, String>,
   ```

3. **`SessionInfo`** — `crates/protocol/src/session.rs:349-424`
   Add after `warnings` (line 416):
   ```rust
   /// Per-session metadata map, set at creation and updatable via
   /// `session.set_metadata`. Owner-controlled; never holds secrets.
   #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
   pub metadata: BTreeMap<String, String>,
   ```

4. **New params/result pair** — `crates/protocol/src/session.rs:~462`
   (after `SessionResizeResult`, matching the `*Params`/`*Result` convention):
   ```rust
   /// Parameters for `session.set_metadata`.
   #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
   pub struct SessionSetMetadataParams {
       /// Session to update.
       pub session_id: SessionId,
       /// Merge map: `Some(v)` sets the key, `None` (JSON null) deletes it,
       /// unmentioned keys are left untouched.
       pub metadata: BTreeMap<String, Option<String>>,
   }

   /// Result returned by `session.set_metadata`.
   #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
   pub struct SessionSetMetadataResult {
       /// Updated session summary after the metadata change.
       pub session: SessionInfo,
   }
   ```

### Daemon crate (`crates/daemon`)

5. **`ResumeBinding`** — `crates/daemon/src/store/mod.rs:56-118`
   Add after `is_linked_worktree` (line 89):
   ```rust
   /// Per-session metadata, persisted so a resumed session restores it after a
   /// daemon restart. Owner-controlled; never a secret (see module doc).
   #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
   pub metadata: BTreeMap<String, String>,
   ```
   Update the manual `Deserialize` impl (`store/mod.rs:120-181`):
   - in `RawResumeBinding` (125-154) add `#[serde(default)] metadata: BTreeMap<String, String>,`
   - in the `Ok(Self { ... })` return (~162) add `metadata: raw.metadata,`
   Also update the module doc-comment at `store/mod.rs:26` re: secrets (above).
   `record_resume` (`store/mod.rs:~411`) is a generic upsert — no signature change.

6. **`SessionEntry`** — `crates/daemon/src/session/mod.rs:287-300`
   No new field: metadata rides inside `info: SessionInfo`. Mutation happens
   in-place via `entry.info.metadata`.

7. **`SessionInfo` construction** — `crates/daemon/src/session/target.rs:389-413`
   In `register_pty_session`, after `warnings` (line 409):
   ```rust
   metadata: params_metadata, // validated metadata threaded from session.new
   ```
   Thread the validated map in from `session.new` through the registry call.

8. **Registry methods** — `crates/daemon/src/session/mod.rs`
   - Add a private `validate_metadata(&BTreeMap<...>) -> Result<(), ...>` choke
     point enforcing the limits; call it from both new and set paths.
   - Add `pub async fn set_metadata(&self, id: &SessionId, merge: BTreeMap<String, Option<String>>) -> Result<SessionSetMetadataResult, _>`:
     look up the entry (not-found error mirrors `resize`/`stop` at ~950), apply the
     merge to `entry.info.metadata`, validate the result, bump `updated_at`, then
     re-persist the resume binding and return the updated `SessionInfo`.

9. **Persist** — `crates/daemon/src/session/resume.rs:42-100`
   In `persist_resume_binding`, in the `ResumeBinding` literal after
   `is_linked_worktree` (line 74):
   ```rust
   metadata: entry.info.metadata.clone(),
   ```
   Ensure `set_metadata` calls this so an updated map is flushed to disk.

10. **RPC dispatch** — `crates/daemon/src/api/handler.rs:151-177`
    Add an arm (after `SESSION_REPORT_NATIVE_ID`):
    ```rust
    method::SESSION_SET_METADATA => {
        handle_session_set_metadata(request, &state.sessions).await
    }
    ```
    Add the handler (template = `handle_session_resize` at `handler.rs:559-571`):
    ```rust
    async fn handle_session_set_metadata(request: &Request, sessions: &SessionRegistry) -> Response {
        let params = match parse_params::<SessionSetMetadataParams>(request) {
            Ok(params) => params,
            Err(err) => return Response::err(request.id.clone(), err),
        };
        match sessions.set_metadata(&params.session_id, params.metadata).await {
            Ok(result) => ok_value(request, &result),
            Err(err) => Response::err(request.id.clone(), err),
        }
    }
    ```

### xtask / generated docs

11. **Protocol descriptor** — `crates/xtask/src/generators/protocol.rs:75-126`
    Add to the `METHODS` slice after `session.report_native_id`:
    ```rust
    MethodDescriptor {
        wire_name: "session.set_metadata",
        description: "Merge a session's metadata map (null value deletes a key).",
    },
    ```
    Regenerate docs (the `session-resize.md`-style reference page is generated).

## Reference: every place `session.resize` appears (mirror for the new method)

- `crates/protocol/src/lib.rs:93` (method constant)
- `crates/protocol/src/session.rs:247, 457` (params/result docs)
- `crates/daemon/src/session/mod.rs:950` (not-found error message)
- `crates/xtask/src/generators/protocol.rs:111` (method descriptor)
- `docs/plan-phase-1.md:99, 112` (documentation)
- `target/pohunek-docs/knowledge-bundle/reference/protocol/session-resize.md` (generated)

## CLI surface (optional, follow-up)

Out of scope for the core change, but a natural follow-up: a `session meta`
subcommand (`set`/`unset`/`show`) and `session list --filter meta.<key>=<value>`.
Track separately so this PR stays focused on the protocol + daemon + store.

## Test plan

- protocol: serde round-trip of `SessionNewParams`/`SessionInfo` with and without
  metadata; `null` in `SessionSetMetadataParams` deserializes to `None`.
- daemon registry: new-with-metadata surfaces on `SessionInfo`; set merge covers
  insert / overwrite / delete / untouched-key; over-limit rejected with a clear
  error; not-found session errors like `resize`.
- store: `ResumeBinding` persists and reloads metadata; a legacy line without the
  field loads with an empty map.
- restart round-trip: persist a binding with metadata, re-read, assert restored.
