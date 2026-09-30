<script lang="ts">
  import type { HostedSession, Workspace } from "@pohunek/client-core";
  import { addErrorToast, addToast, hasKnownSessionAgentBases } from "../lib";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import FormDialog from "./FormDialog.svelte";

  interface Props {
    workspace: Workspace;
    entry: HostedSession;
    onfork: (host: string, sessionId: string) => void;
    onremove: () => void;
  }

  let { workspace, entry, onfork, onremove }: Props = $props();
  let renameOpen = $state(false);
  let forkOpen = $state(false);
  let confirmStop = $state(false);
  let confirmRemove = $state(false);
  let nameDraft = $state("");
  let forkCols = $state(80);
  let forkRows = $state(24);
  let pending = $state(false);
  let generation = 0;

  const session = $derived(entry.session);
  // Unknown future agents are presentation-only until this client understands
  // their mutation semantics. Resume and fork remain capability-driven below.
  const writable = $derived(
    session.external !== true
      && hasKnownSessionAgentBases(session.agent_base, session.active_agent_base),
  );
  const canStop = $derived(
    session.runtime === undefined
      ? session.state === "running" || session.state === "starting"
      : session.runtime.state === "starting"
        || session.runtime.state === "live"
        || session.runtime.state === "reconnecting",
  );
  const canRecover = $derived(isResumableState(session));
  const canResume = $derived(canResumeSession(session));
  const canFork = $derived(session.capabilities.fork);

  function openRename(): void {
    nameDraft = session.name ?? "";
    renameOpen = true;
  }

  function openFork(): void {
    nameDraft = "";
    forkCols = session.cols;
    forkRows = session.rows;
    forkOpen = true;
  }

  async function rename(): Promise<void> {
    if (nameDraft.trim().length === 0) {
      addToast("error", "Session name must not be empty");
      return;
    }
    await mutate(async (): Promise<void> => {
      await workspace.actions.sessionRename(entry.host, {
        session_id: session.id,
        name: nameDraft.trim(),
      });
      renameOpen = false;
      addToast("success", "Session renamed");
    });
  }

  async function clearName(): Promise<void> {
    await mutate(async (): Promise<void> => {
      await workspace.actions.sessionRename(entry.host, { session_id: session.id });
      addToast("success", "Session name cleared");
    });
  }

  async function stop(): Promise<void> {
    await mutate(async (): Promise<void> => {
      const result = await workspace.actions.sessionStop(entry.host, session.id);
      confirmStop = false;
      addToast("success", result.stopped ? "Session stopped" : "Session was already stopped");
    });
  }

  async function resume(): Promise<void> {
    const currentSession = entry.session;
    if (pending || !canResumeSession(currentSession)) {
      return;
    }
    await mutate(async (): Promise<void> => {
      await workspace.actions.sessionResume(entry.host, currentSession.id);
      addToast("success", "Session resumed");
    });
  }

  function isResumableState(candidate: HostedSession["session"]): boolean {
    if (candidate.runtime !== undefined) {
      return candidate.runtime.state === "lost" || candidate.runtime.state === "terminal";
    }
    return candidate.state === "done" || candidate.state === "failed" || candidate.state === "stopped";
  }

  function hasNativeReference(candidate: HostedSession["session"]): boolean {
    return (candidate.native_session_id?.length ?? 0) > 0
      || (candidate.native_session_path?.length ?? 0) > 0;
  }

  function canResumeSession(candidate: HostedSession["session"]): boolean {
    return candidate.capabilities.resume && isResumableState(candidate) && hasNativeReference(candidate);
  }

  async function fork(): Promise<void> {
    await mutate(async (): Promise<void> => {
      const result = await workspace.actions.sessionFork(entry.host, {
        session_id: session.id,
        cwd_mode: "same",
        cols: forkCols,
        rows: forkRows,
        ...(nameDraft.trim().length === 0 ? {} : { name: nameDraft.trim() }),
      });
      forkOpen = false;
      addToast("success", "Session forked");
      onfork(entry.host, result.id);
    });
  }

  async function remove(): Promise<void> {
    await mutate(async (): Promise<void> => {
      const result = await workspace.actions.sessionRemove(entry.host, session.id);
      confirmRemove = false;
      addToast("success", result.stopped ? "Session stopped and removed" : "Session removed");
      onremove();
    });
  }

  async function mutate(operation: () => Promise<void>): Promise<void> {
    const current = generation + 1;
    generation = current;
    pending = true;
    try {
      await operation();
    } catch (error: unknown) {
      if (current === generation) {
        addErrorToast(error);
      }
    } finally {
      if (current === generation) {
        pending = false;
      }
    }
  }
</script>

{#if writable}
  <div class="session-lifecycle-actions" aria-label="Session actions">
    <button type="button" onclick={openRename} disabled={pending}>Rename</button>
    {#if session.name !== undefined}
      <button type="button" onclick={() => void clearName()} disabled={pending}>Clear name</button>
    {/if}
    {#if canStop}
      <button type="button" onclick={() => { confirmStop = true; }} disabled={pending}>Stop</button>
    {:else if canResume}
      <button type="button" onclick={() => void resume()} disabled={pending}>Resume</button>
    {:else if canRecover && !session.capabilities.resume}
      <button type="button" disabled title="This session's agent adapter does not support native resume">
        Resume unsupported
      </button>
    {:else if canRecover}
      <button type="button" disabled title="This session has no usable native resume reference">
        Resume unavailable
      </button>
    {:else}
      <button type="button" disabled title="Resolve the runtime conflict or incompatibility before recovery">
        Resume unavailable
      </button>
    {/if}
    <button
      type="button"
      onclick={openFork}
      disabled={pending || !canFork}
      title={canFork ? "Fork this native agent conversation" : "This session's agent adapter does not support native fork"}
    >{canFork ? "Fork" : "Fork unsupported"}</button>
    <button class="button-danger" type="button" onclick={() => { confirmRemove = true; }} disabled={pending}>
      Remove
    </button>
  </div>
{/if}

<ConfirmDialog
  open={confirmStop}
  title="Stop this session?"
  message="The agent process will stop. Existing terminal views will close."
  confirmLabel="Stop session"
  onconfirm={() => void stop()}
  oncancel={() => { confirmStop = false; }}
/>

<ConfirmDialog
  open={confirmRemove}
  title="Remove this session?"
  message={session.state === "running" ? "This permanently removes the session and also stops its live PTY." : "This permanently removes the session."}
  confirmLabel="Remove session"
  onconfirm={() => void remove()}
  oncancel={() => { confirmRemove = false; }}
/>

<FormDialog
  open={renameOpen || forkOpen}
  title={renameOpen ? "Rename session" : "Fork session"}
  submitting={pending}
  onclose={() => {
    renameOpen = false;
    forkOpen = false;
  }}
  onsubmit={() => {
    if (renameOpen) {
      void rename();
    } else {
      void fork();
    }
  }}
>
  {#if renameOpen}
    <label>Name<input bind:value={nameDraft} /></label>
  {:else}
    <label>Display name (optional)<input bind:value={nameDraft} /></label>
    <label>Columns<input type="number" min="1" bind:value={forkCols} /></label>
    <label>Rows<input type="number" min="1" bind:value={forkRows} /></label>
  {/if}
</FormDialog>

<style>
  .session-lifecycle-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
</style>
