<script lang="ts">
  import { hostResourceKey, type SessionsSnapshot, type Workspace } from "@pohunek/client-core";
  import { onDestroy, tick } from "svelte";
  import type { Readable } from "svelte/store";
  import { addErrorToast, addToast } from "../lib";
  import AgentBadge from "./AgentBadge.svelte";
  import ConfirmDialog from "./ConfirmDialog.svelte";

  interface Props {
    open: boolean;
    workspace: Workspace;
    sessions: Readable<SessionsSnapshot>;
    host: string;
    sessionId: string;
    onclose: () => void;
    onopenterminal: (host: string, sessionId: string) => void;
  }

  type Detail = Awaited<ReturnType<Workspace["actions"]["sessionInspect"]>>;

  let { open, workspace, sessions, host, sessionId, onclose, onopenterminal }: Props = $props();
  let detail: Detail | undefined = $state();
  let detailKey = "";
  let loading = $state(false);
  let inspectFailed = $state(false);
  let confirmStop = $state(false);
  let stopping = $state(false);
  let inspectGeneration = 0;
  let actionGeneration = 0;
  let drawer = $state<HTMLDivElement>();
  let closeButton = $state<HTMLButtonElement>();
  let previouslyFocused: HTMLElement | undefined;
  let focusOwned = false;

  $effect((): void => {
    const live = $sessions[hostResourceKey(host, sessionId)];
    if (live !== undefined) {
      detail = live.session;
      detailKey = hostResourceKey(host, sessionId);
    }
  });

  $effect((): void => {
    if (!open) {
      return;
    }
    const key = hostResourceKey(host, sessionId);
    if (detailKey !== key) {
      detail = undefined;
      detailKey = key;
    }
    void inspect(host, sessionId);
  });

  $effect((): void => {
    if (open && !focusOwned) {
      focusOwned = true;
      previouslyFocused = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
      void tick().then((): void => {
        if (open) {
          closeButton?.focus();
        }
      });
    } else if (!open) {
      inspectGeneration += 1;
      actionGeneration += 1;
      loading = false;
      stopping = false;
      restoreFocus();
    }
  });

  onDestroy((): void => {
    inspectGeneration += 1;
    actionGeneration += 1;
    restoreFocus();
  });

  async function inspect(inspectHost = host, inspectSessionId = sessionId): Promise<void> {
    const generation = inspectGeneration + 1;
    inspectGeneration = generation;
    loading = true;
    inspectFailed = false;
    try {
      const inspected = await workspace.actions.sessionInspect(inspectHost, inspectSessionId);
      if (generation === inspectGeneration) {
        detail = inspected;
      }
    } catch (error: unknown) {
      if (generation === inspectGeneration) {
        inspectFailed = true;
        addErrorToast(error);
      }
    } finally {
      if (generation === inspectGeneration) {
        loading = false;
      }
    }
  }

  async function stopSession(): Promise<void> {
    const generation = actionGeneration + 1;
    actionGeneration = generation;
    stopping = true;
    try {
      const result = await workspace.actions.sessionStop(host, sessionId);
      if (generation !== actionGeneration || !open) {
        return;
      }
      addToast("success", result.stopped ? "Session stopped" : "Session was already stopped");
      await inspect();
    } catch (error: unknown) {
      if (generation === actionGeneration && open) {
        addErrorToast(error);
      }
    } finally {
      if (generation === actionGeneration) {
        stopping = false;
      }
    }
  }

  function restoreFocus(): void {
    if (!focusOwned) {
      return;
    }
    focusOwned = false;
    const target = previouslyFocused;
    previouslyFocused = undefined;
    void tick().then((): void => {
      if (target?.isConnected === true) {
        target.focus();
      }
    });
  }

  function trapDrawerFocus(event: KeyboardEvent): void {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      onclose();
      return;
    }
    if (event.key !== "Tab") {
      return;
    }

    const activeDrawer = drawer;
    if (activeDrawer === undefined) {
      return;
    }
    const focusable = focusableElements(activeDrawer);
    if (focusable.length === 0) {
      event.preventDefault();
      activeDrawer.focus();
      return;
    }
    const first = focusable[0];
    const last = focusable.at(-1);
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last?.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first?.focus();
    }
  }

  function focusableElements(container: HTMLElement): readonly HTMLElement[] {
    return Array.from(container.querySelectorAll<HTMLElement>(
      "button:not([disabled]), a[href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), summary, [tabindex]:not([tabindex='-1'])",
    )).filter((element) => element.getClientRects().length > 0);
  }
</script>

{#if open}
  <button
    class="drawer-backdrop"
    type="button"
    tabindex="-1"
    aria-label="Close session details"
    onclick={onclose}
  ></button>
  <div
    bind:this={drawer}
    class="drawer"
    role="dialog"
    aria-modal="true"
    aria-labelledby="session-inspector-title"
    tabindex="-1"
    onkeydown={trapDrawerFocus}
  >
    <header class="drawer-heading">
      <div>
        <h2 id="session-inspector-title">{detail?.name ?? sessionId}</h2>
        <p>{host}</p>
      </div>
      <button
        bind:this={closeButton}
        class="icon-button"
        type="button"
        aria-label="Close session details"
        onclick={onclose}
      >×</button>
    </header>

    {#if loading && detail === undefined}
      <p class="state-message">Inspecting session…</p>
    {:else if inspectFailed && detail === undefined}
      <section class="error-panel">
        <h3>Session unavailable</h3>
        <p>The host could not inspect this session.</p>
        <button type="button" onclick={() => void inspect()}>Retry</button>
      </section>
    {:else if detail !== undefined}
      <section class="operational" data-testid="session-detail">
        <div class="state-row">
          {#if detail.activity !== undefined}
            <AgentBadge activity={detail.activity} source={detail.state_source} />
          {:else}
            <span class="unknown-activity">Activity unknown</span>
          {/if}
          <span class={`lifecycle lifecycle-${detail.state}`}>{detail.state}</span>
        </div>

        <dl>
          <div><dt>Project</dt><dd>{detail.project_label ?? detail.project_id ?? "No project"}</dd></div>
          {#if detail.branch !== undefined}<div><dt>Branch</dt><dd>{detail.branch}</dd></div>{/if}
          {#if detail.repo !== undefined}<div><dt>Repository</dt><dd>{detail.repo}</dd></div>{/if}
          {#if detail.worktree_path !== undefined}<div><dt>Worktree</dt><dd>{detail.worktree_path}</dd></div>{/if}
          <div><dt>Agent</dt><dd>{detail.active_agent ?? detail.agent}</dd></div>
          <div><dt>Host</dt><dd>{host}</dd></div>
        </dl>

        {#if detail.warnings !== undefined && detail.warnings.length > 0}
          <div class="warnings">
            <strong>Setup warnings</strong>
            <ul>
              {#each detail.warnings as warning (`${warning.kind}:${warning.message}`)}
                <li>{warning.message}</li>
              {/each}
            </ul>
          </div>
        {/if}

        <div class="primary-actions">
          <button
            class="button-primary"
            type="button"
            disabled={detail.state !== "running" || detail.external === true}
            onclick={() => onopenterminal(host, sessionId)}
          >
            Open terminal
          </button>
          <button
            class="button-danger"
            type="button"
            disabled={stopping || detail.state === "stopped" || detail.external === true}
            onclick={() => { confirmStop = true; }}
          >
            {stopping ? "Stopping…" : "Stop session"}
          </button>
        </div>

        <details class="technical-details">
          <summary>Technical details</summary>
          <dl>
            <div><dt>Session ID</dt><dd>{detail.id}</dd></div>
            <div><dt>PID</dt><dd>{detail.pid}</dd></div>
            <div><dt>Working directory</dt><dd>{detail.cwd}</dd></div>
            <div><dt>Terminal</dt><dd>{detail.cols} × {detail.rows}</dd></div>
            <div><dt>State source</dt><dd>{detail.state_source}</dd></div>
            <div><dt>Created</dt><dd>{detail.created_at}</dd></div>
            <div><dt>Updated</dt><dd>{detail.updated_at}</dd></div>
            {#if detail.exit_code !== undefined}<div><dt>Exit code</dt><dd>{detail.exit_code}</dd></div>{/if}
            {#if detail.native_session_id !== undefined}
              <div><dt>Native session ID</dt><dd>{detail.native_session_id}</dd></div>
            {/if}
          </dl>
        </details>
      </section>
    {/if}
  </div>
{/if}

<ConfirmDialog
  open={confirmStop}
  title="Stop this session?"
  message="The agent process will stop. Existing terminal views will close."
  confirmLabel="Stop session"
  onconfirm={() => { confirmStop = false; void stopSession(); }}
  oncancel={() => { confirmStop = false; }}
/>

<style>
  .drawer-backdrop {
    position: fixed;
    z-index: 39;
    inset: 0;
    width: 100%;
    height: 100%;
    padding: 0;
    border: 0;
    border-radius: 0;
    background: rgb(0 0 0 / 55%);
  }

  .drawer {
    position: fixed;
    z-index: 40;
    top: 0;
    right: 0;
    bottom: 0;
    width: min(32rem, 100vw);
    overflow: auto;
    padding: 1.25rem;
    border-left: 1px solid var(--border);
    background: var(--surface-raised);
    box-shadow: -1rem 0 3rem rgb(0 0 0 / 35%);
  }

  .drawer-heading,
  .state-row,
  .primary-actions {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
  }

  .drawer-heading {
    align-items: flex-start;
    margin-bottom: 1.25rem;
  }

  h2,
  h3,
  p {
    margin-bottom: 0;
  }

  .drawer-heading p,
  .state-message {
    color: var(--muted);
  }

  .icon-button {
    min-width: 2.25rem;
    padding: 0;
    font-size: 1.35rem;
  }

  .operational {
    display: grid;
    gap: 1.25rem;
  }

  .lifecycle,
  .unknown-activity {
    padding: 0.25rem 0.55rem;
    border: 1px solid var(--border);
    border-radius: 999px;
    color: var(--muted);
    font-size: 0.75rem;
  }

  dl {
    display: grid;
    gap: 0.8rem;
    margin: 0;
  }

  dl div {
    min-width: 0;
  }

  dt {
    margin-bottom: 0.15rem;
    color: var(--muted);
    font-size: 0.72rem;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }

  dd {
    overflow-wrap: anywhere;
    margin: 0;
  }

  .primary-actions {
    justify-content: flex-start;
  }

  .warnings,
  .error-panel {
    padding: 0.85rem;
    border: 1px solid #765939;
    border-radius: 0.5rem;
    background: #2a2117;
  }

  .warnings ul {
    margin-bottom: 0;
    padding-left: 1.25rem;
  }

  .technical-details {
    padding-top: 1rem;
    border-top: 1px solid var(--border);
  }

  .technical-details summary {
    margin-bottom: 1rem;
    color: var(--muted);
    cursor: pointer;
  }
</style>
