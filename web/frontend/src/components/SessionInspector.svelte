<script lang="ts">
  import {
    hostResourceKey,
    type RuntimeContinuity,
    type SessionsSnapshot,
    type Workspace,
  } from "@pohunek/client-core";
  import { onDestroy, tick } from "svelte";
  import type { Readable } from "svelte/store";
  import {
    addErrorToast,
    addToast,
    agentProfileLabel,
    LatestRequest,
    structuredErrorDetails,
  } from "../lib";
  import AgentBadge from "./AgentBadge.svelte";
  import FormDialog from "./FormDialog.svelte";
  import SessionLifecycleActions from "./SessionLifecycleActions.svelte";

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
  type Capabilities = Awaited<ReturnType<Workspace["actions"]["hostInspect"]>>;
  type Screen = Awaited<ReturnType<Workspace["actions"]["sessionScreen"]>>;
  type Output = Awaited<ReturnType<Workspace["actions"]["sessionOutput"]>>;

  const OUTPUT_READ_BYTES = 16_384;

  let { open, workspace, sessions, host, sessionId, onclose, onopenterminal }: Props = $props();
  let detail: Detail | undefined = $state();
  let capabilities: Capabilities | undefined = $state();
  let screen: Screen | undefined = $state();
  let outputCursor: Pick<Output, "runtime_id" | "runtime_generation" | "next_offset"> | undefined = $state();
  let outputText = $state("");
  let outputNotice: string | undefined = $state();
  let observationLoading = $state(false);
  let observationKey = "";
  let capabilityKey = "";
  let detailKey = "";
  let runtimeContinuity: RuntimeContinuity = $state("initial");
  let loading = $state(false);
  let inspectFailed = $state(false);
  let metadataOpen = $state(false);
  let metadataKey = $state("");
  let metadataValue = $state("");
  let mutating = $state(false);
  let inspectGeneration = 0;
  let actionGeneration = 0;
  const observationRequests = new LatestRequest();
  const capabilityRequests = new LatestRequest();
  let drawer = $state<HTMLDivElement>();
  let closeButton = $state<HTMLButtonElement>();
  let previouslyFocused: HTMLElement | undefined;
  let focusOwned = false;

  $effect((): void => {
    const live = $sessions[hostResourceKey(host, sessionId)];
    if (live !== undefined) {
      detail = live.session;
      detailKey = hostResourceKey(host, sessionId);
      runtimeContinuity = live.runtimeContinuity;
    }
  });

  $effect((): void => {
    if (!open) {
      return;
    }
    const key = hostResourceKey(host, sessionId);
    if (observationKey !== key) {
      observationRequests.invalidate();
      observationKey = key;
      observationLoading = false;
      screen = undefined;
      outputCursor = undefined;
      outputText = "";
      outputNotice = undefined;
    }
    if (detailKey !== key) {
      detail = undefined;
      detailKey = key;
    }
    if (capabilityKey !== host) {
      capabilityRequests.invalidate();
      capabilityKey = host;
      capabilities = undefined;
    }
    void inspect(host, sessionId);
    void loadCapabilities(host);
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
      observationRequests.invalidate();
      capabilityRequests.invalidate();
      observationLoading = false;
      loading = false;
      restoreFocus();
    }
  });

  onDestroy((): void => {
    inspectGeneration += 1;
    actionGeneration += 1;
    observationRequests.invalidate();
    capabilityRequests.invalidate();
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

  async function loadCapabilities(inspectHost: string): Promise<void> {
    const token = capabilityRequests.begin(inspectHost);
    try {
      const inspected = await workspace.actions.hostInspect(inspectHost);
      if (open && capabilityRequests.isCurrent(token, host)) {
        capabilities = inspected;
      }
    } catch (error: unknown) {
      if (open && capabilityRequests.isCurrent(token, host)) {
        capabilities = undefined;
        addErrorToast(error);
      }
    }
  }

  async function loadScreen(): Promise<void> {
    const requestHost = host;
    const requestSessionId = sessionId;
    const requestKey = hostResourceKey(requestHost, requestSessionId);
    const token = observationRequests.begin(requestKey);
    observationLoading = true;
    try {
      const result = await workspace.actions.sessionScreen(requestHost, { session_id: requestSessionId });
      if (!open || !observationRequests.isCurrent(token, observationKey)) return;
      screen = result;
    } catch (error: unknown) {
      if (open && observationRequests.isCurrent(token, observationKey)) {
        addErrorToast(error);
      }
    } finally {
      if (observationRequests.isCurrent(token, observationKey)) {
        observationLoading = false;
      }
    }
  }

  async function loadOutput(): Promise<void> {
    const requestHost = host;
    const requestSessionId = sessionId;
    const requestKey = hostResourceKey(requestHost, requestSessionId);
    const requestCursor = outputCursor;
    const token = observationRequests.begin(requestKey);
    observationLoading = true;
    outputNotice = undefined;
    try {
      const result = await workspace.actions.sessionOutput(requestHost, {
        session_id: requestSessionId,
        ...(requestCursor === undefined ? {} : {
          runtime: {
            runtime_id: requestCursor.runtime_id,
            runtime_generation: requestCursor.runtime_generation,
          },
          after_offset: requestCursor.next_offset,
        }),
        max_bytes: OUTPUT_READ_BYTES,
      });
      if (!open || !observationRequests.isCurrent(token, observationKey)) return;
      if (result.gap !== undefined) {
        outputNotice = `Output ${result.gap.start_offset}–${result.gap.end_offset} is no longer retained.`;
        outputText = "";
      }
      outputText += decodeOutput(result.data_base64);
      outputCursor = {
        runtime_id: result.runtime_id,
        runtime_generation: result.runtime_generation,
        next_offset: result.next_offset,
      };
    } catch (error: unknown) {
      if (!open || !observationRequests.isCurrent(token, observationKey)) return;
      if (structuredErrorDetails(error)?.code === "session_runtime_changed") {
        outputCursor = undefined;
        outputText = "";
        outputNotice = "The runtime changed. Output cursor was reset; read again from the current runtime.";
      } else {
        addErrorToast(error);
      }
    } finally {
      if (observationRequests.isCurrent(token, observationKey)) {
        observationLoading = false;
      }
    }
  }

  function decodeOutput(encoded: string): string {
    const binary = atob(encoded);
    const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0));
    return new TextDecoder().decode(bytes);
  }

  async function updateMetadata(value: string | null): Promise<void> {
    const key = metadataKey.trim();
    if (key.length === 0) {
      addToast("error", "Metadata key must not be empty");
      return;
    }
    await runMutation(async (): Promise<void> => {
      await workspace.actions.sessionSetMetadata(host, {
        session_id: sessionId,
        metadata: { [key]: value },
      });
      metadataOpen = false;
      metadataKey = "";
      metadataValue = "";
      addToast("success", value === null ? "Metadata removed" : "Metadata saved");
      await inspect();
    });
  }

  async function runMutation(operation: () => Promise<void>): Promise<void> {
    const generation = actionGeneration + 1;
    actionGeneration = generation;
    mutating = true;
    try {
      await operation();
    } catch (error: unknown) {
      if (generation === actionGeneration && open) {
        addErrorToast(error);
      }
    } finally {
      if (generation === actionGeneration) {
        mutating = false;
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
        {#if detail.runtime !== undefined}
          <div
            class={`runtime-state runtime-${detail.runtime.state}`}
            data-testid="runtime-state"
            data-runtime-continuity={runtimeContinuity}
          >
            <strong>Runtime {detail.runtime.state}</strong>
            {#if runtimeContinuity === "recovered"}
              <span>A new PTY generation replaced the previous runtime.</span>
            {:else if runtimeContinuity === "reconnected"}
              <span>The daemon reconnected to the same PTY generation.</span>
            {:else if detail.runtime.loss_reason !== undefined}
              <span>{detail.runtime.loss_reason}</span>
            {/if}
          </div>
        {/if}

        <dl>
          <div><dt>Project</dt><dd>{detail.project_label ?? detail.project_id ?? "No project"}</dd></div>
          {#if detail.branch !== undefined}<div><dt>Branch</dt><dd>{detail.branch}</dd></div>{/if}
          {#if detail.repo !== undefined}<div><dt>Repository</dt><dd>{detail.repo}</dd></div>{/if}
          {#if detail.worktree_path !== undefined}<div><dt>Worktree</dt><dd>{detail.worktree_path}</dd></div>{/if}
          <div><dt>Agent</dt><dd>{detail.active_agent !== undefined && detail.active_agent_base !== undefined
            ? agentProfileLabel(detail.active_agent, detail.active_agent_base)
            : agentProfileLabel(detail.agent, detail.agent_base)}</dd></div>
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
          {#if detail.external !== true}
            <button
              class="button-primary"
              type="button"
              disabled={detail.state !== "running"
                || (detail.runtime !== undefined
                  && detail.runtime.state !== "live"
                  && detail.runtime.state !== "reconnecting")}
              onclick={() => onopenterminal(host, sessionId)}
            >
              Open terminal
            </button>
          {/if}
        </div>

        {#if capabilities?.terminal_read_supported === true || capabilities?.output_read_supported === true}
          <section class="observation" aria-label="Session observation">
            <div class="observation-heading">
              <h3>Terminal observation</h3>
              <div>
                {#if capabilities.terminal_read_supported}
                  <button type="button" disabled={observationLoading} onclick={() => void loadScreen()}>
                    Read screen
                  </button>
                {/if}
                {#if capabilities.output_read_supported}
                  <button type="button" disabled={observationLoading} onclick={() => void loadOutput()}>
                    {outputCursor === undefined ? "Read output" : "Read new output"}
                  </button>
                {/if}
              </div>
            </div>
            {#if outputNotice !== undefined}<p class="observation-notice">{outputNotice}</p>{/if}
            {#if screen !== undefined}
              <div class="terminal-snapshot" data-testid="terminal-screen">
                <span>Screen {screen.dimensions.cols} × {screen.dimensions.rows} · watermark {screen.watermark}</span>
                <pre>{screen.visible_lines.join("\n")}</pre>
              </div>
            {/if}
            {#if outputText.length > 0}
              <div class="terminal-snapshot" data-testid="terminal-output">
                <span>Output through offset {outputCursor?.next_offset}</span>
                <pre>{outputText}</pre>
              </div>
            {/if}
          </section>
        {/if}

        {#if detail.external !== true}
          <section class="management-actions" aria-label="Session management">
            <h3>Manage session</h3>
            <SessionLifecycleActions
              {workspace}
              entry={{ host, session: detail, attachStreamIds: [], runtimeContinuity }}
              onfork={onopenterminal}
              onremove={onclose}
            />
            <div class="metadata">
              <h4>Metadata</h4>
              {#each Object.entries(detail.metadata ?? {}) as [key, value] (key)}
                <div>
                  <code>{key}</code>
                  <span>{value}</span>
                  <button
                    type="button"
                    onclick={() => {
                      metadataKey = key;
                      void updateMetadata(null);
                    }}
                    disabled={mutating}
                  >
                    Clear
                  </button>
                </div>
              {:else}
                <p>No metadata.</p>
              {/each}
              <button
                type="button"
                onclick={() => {
                  metadataKey = "";
                  metadataValue = "";
                  metadataOpen = true;
                }}
                disabled={mutating}
              >
                Set metadata
              </button>
            </div>
          </section>
        {:else}
          <p class="read-only">External observed sessions are read-only.</p>
        {/if}

        <details class="technical-details">
          <summary>Technical details</summary>
          <dl>
            <div><dt>Session ID</dt><dd>{detail.id}</dd></div>
            <div><dt>PID</dt><dd>{detail.pid}</dd></div>
            <div><dt>Working directory</dt><dd>{detail.cwd}</dd></div>
            <div><dt>Terminal</dt><dd>{detail.cols} × {detail.rows}</dd></div>
            <div><dt>State source</dt><dd>{detail.state_source}</dd></div>
            {#if detail.runtime !== undefined}
              <div><dt>Runtime state</dt><dd>{detail.runtime.state}</dd></div>
              {#if detail.runtime.runtime_id !== undefined}
                <div><dt>Runtime ID</dt><dd>{detail.runtime.runtime_id}</dd></div>
              {/if}
              {#if detail.runtime.worker_id !== undefined}
                <div><dt>Worker ID</dt><dd>{detail.runtime.worker_id}</dd></div>
              {/if}
            {/if}
            <div><dt>Created</dt><dd>{detail.created_at}</dd></div>
            <div><dt>Updated</dt><dd>{detail.updated_at}</dd></div>
          {#if detail.exit_code !== undefined}
            <div><dt>Exit code</dt><dd>{detail.exit_code}</dd></div>
          {/if}
            {#if detail.native_session_id !== undefined}
              <div><dt>Native session ID</dt><dd>{detail.native_session_id}</dd></div>
            {/if}
          </dl>
        </details>
      </section>
    {/if}
  </div>
{/if}

<FormDialog
  open={metadataOpen}
  title="Set metadata"
  submitting={mutating}
  onclose={() => { metadataOpen = false; }}
  onsubmit={() => { void updateMetadata(metadataValue); }}
>
  <label>Key<input bind:value={metadataKey} /></label>
  <label>Value<input bind:value={metadataValue} /></label>
</FormDialog>

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

  .runtime-state {
    display: grid;
    gap: 0.25rem;
    padding: 0.75rem;
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    background: var(--surface);
  }

  .runtime-lost,
  .runtime-conflict,
  .runtime-incompatible {
    border-color: var(--danger);
  }

  .management-actions,
  .metadata {
    display: grid;
    gap: 0.75rem;
  }

  .metadata > div {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 2fr) auto;
    gap: 0.5rem;
    align-items: center;
  }

  .read-only { color: var(--muted); }

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

  .observation,
  .terminal-snapshot {
    display: grid;
    gap: 0.75rem;
  }

  .observation-heading {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
  }

  .observation-heading div {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }

  .observation-notice {
    color: var(--warning);
  }

  .terminal-snapshot {
    min-width: 0;
    padding: 0.75rem;
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    background: var(--surface);
  }

  .terminal-snapshot span {
    color: var(--muted);
    font-size: 0.75rem;
  }

  .terminal-snapshot pre {
    overflow: auto;
    max-height: 18rem;
    margin: 0;
    white-space: pre-wrap;
    word-break: break-word;
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
