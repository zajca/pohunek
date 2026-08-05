<script lang="ts">
  import type { HostsSnapshot, Workspace } from "@pohunek/client-core";
  import { onDestroy } from "svelte";
  import type { Readable } from "svelte/store";
  import { addErrorToast, agentRuntimeLabel, isLaunchableRuntime } from "../lib";
  import TerminalSizeProbe from "./TerminalSizeProbe.svelte";

  export interface TerminalSize {
    readonly cols: number;
    readonly rows: number;
  }

  interface Props {
    open: boolean;
    workspace: Workspace;
    hosts: Readable<HostsSnapshot>;
    selectedHost?: string | undefined;
    terminalSize?: TerminalSize | undefined;
    onclose: () => void;
    oncreated: (host: string, sessionId: string) => void;
  }

  type Capabilities = Awaited<ReturnType<Workspace["actions"]["hostInspect"]>>;
  type Projects = Awaited<ReturnType<Workspace["actions"]["projectList"]>>;
  type NewSessionParams = Parameters<Workspace["actions"]["sessionNew"]>[1];

  let {
    open,
    workspace,
    hosts,
    selectedHost: preferredHost,
    terminalSize: providedTerminalSize,
    onclose,
    oncreated,
  }: Props = $props();
  let dialog: HTMLDialogElement;
  let selectedHost = $state("");
  let selectedAgent = $state("");
  let sessionName = $state("");
  let selectedProject = $state("");
  let branch = $state("");
  let measuredTerminalSize: TerminalSize | undefined = $state();
  let capabilities: Capabilities | undefined = $state();
  let projects: Projects = $state([]);
  let optionsLoading = $state(false);
  let projectsUnavailable = $state(false);
  let submitting = $state(false);
  let optionsGeneration = 0;
  let submitGeneration = 0;

  const connectedHosts = $derived(
    Object.values($hosts)
      .filter((host) => host.connection.kind === "connected")
      .sort((left, right) => left.host.localeCompare(right.host)),
  );
  const launchTerminalSize = $derived(providedTerminalSize ?? measuredTerminalSize);

  $effect((): void => {
    if (open && !dialog.open) {
      dialog.showModal();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  });

  $effect((): void => {
    if (!open) {
      optionsGeneration += 1;
      submitGeneration += 1;
      optionsLoading = false;
      submitting = false;
    }
  });

  onDestroy((): void => {
    optionsGeneration += 1;
    submitGeneration += 1;
  });

  $effect((): void => {
    if (!open || selectedHost.length > 0) {
      return;
    }
    if (preferredHost !== undefined && connectedHosts.some((host) => host.host === preferredHost)) {
      selectedHost = preferredHost;
    } else if (connectedHosts[0] !== undefined) {
      selectedHost = connectedHosts[0].host;
    }
  });

  $effect((): void => {
    const host = selectedHost;
    if (open && host.length > 0) {
      void loadHostOptions(host);
    }
  });

  async function loadHostOptions(host: string): Promise<void> {
    const generation = optionsGeneration + 1;
    optionsGeneration = generation;
    optionsLoading = true;
    capabilities = undefined;
    projects = [];
    projectsUnavailable = false;
    selectedAgent = "";
    selectedProject = "";
    branch = "";

    try {
      const inspected = await workspace.actions.hostInspect(host);
      if (optionsGeneration === generation) {
        capabilities = inspected;
        selectedAgent = inspected.supported_agents.find((agent) => runtimeLaunchable(inspected, agent)) ?? "";
      }
    } catch (error: unknown) {
      if (optionsGeneration === generation) {
        addErrorToast(error);
      }
    }

    if (optionsGeneration !== generation) {
      return;
    }

    try {
      const listedProjects = await workspace.actions.projectList(host, {});
      if (optionsGeneration === generation) {
        projects = listedProjects;
      }
    } catch (error: unknown) {
      if (optionsGeneration === generation) {
        projectsUnavailable = true;
        addErrorToast(error);
      }
    } finally {
      if (optionsGeneration === generation) {
        optionsLoading = false;
      }
    }
  }

  function runtimeFor(snapshot: Capabilities, agent: string): Capabilities["runtimes"][number] | undefined {
    return snapshot.runtimes.find((runtime) => runtime.agent === agent);
  }

  function runtimeLaunchable(snapshot: Capabilities, agent: string): boolean {
    return isLaunchableRuntime(agent, runtimeFor(snapshot, agent));
  }

  function onProjectChange(): void {
    if (selectedProject.length === 0) {
      branch = "";
    }
  }

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (launchTerminalSize === undefined || selectedHost.length === 0 || selectedAgent.length === 0) {
      return;
    }

    const normalizedName = sessionName.trim();
    const normalizedBranch = branch.trim();
    const params: NewSessionParams = {
      agent: selectedAgent,
      cols: launchTerminalSize.cols,
      rows: launchTerminalSize.rows,
      ...(normalizedName.length === 0 ? {} : { name: normalizedName }),
      ...(selectedProject.length === 0 ? {} : { project: selectedProject }),
      ...(selectedProject.length === 0 || normalizedBranch.length === 0 ? {} : { branch: normalizedBranch }),
    };

    const generation = submitGeneration + 1;
    submitGeneration = generation;
    submitting = true;
    try {
      const created = await workspace.actions.sessionNew(selectedHost, params);
      if (generation === submitGeneration && open) {
        oncreated(selectedHost, created.id);
      }
    } catch (error: unknown) {
      if (generation === submitGeneration && open) {
        addErrorToast(error);
      }
    } finally {
      if (generation === submitGeneration) {
        submitting = false;
      }
    }
  }
</script>

<dialog
  bind:this={dialog}
  class="new-session-dialog"
  aria-labelledby="new-session-title"
  oncancel={(event) => { event.preventDefault(); onclose(); }}
>
  <header class="dialog-heading">
    <div>
      <h2 id="new-session-title">New session</h2>
      <p>Start an agent on any connected host.</p>
    </div>
    <button class="icon-button" type="button" aria-label="Close new session" onclick={onclose}>×</button>
  </header>

  <form onsubmit={(event) => void submit(event)}>
    <div class="form-grid">
      <label>
        <span>Host</span>
        <select bind:value={selectedHost} required>
          {#if connectedHosts.length === 0}
            <option value="">No connected hosts</option>
          {:else}
            {#each connectedHosts as host (host.host)}
              <option value={host.host}>{host.host}</option>
            {/each}
          {/if}
        </select>
      </label>

      <label>
        <span>Agent</span>
        <select bind:value={selectedAgent} required disabled={capabilities === undefined}>
          {#if optionsLoading && capabilities === undefined}
            <option value="">Inspecting host…</option>
          {:else if capabilities !== undefined}
            {#each capabilities.supported_agents as agent (agent)}
              <option value={agent} disabled={!runtimeLaunchable(capabilities, agent)}>
                {agentRuntimeLabel(agent, runtimeFor(capabilities, agent))}
              </option>
            {/each}
          {/if}
        </select>
      </label>

      <label>
        <span>Project <small>optional</small></span>
        <select bind:value={selectedProject} onchange={onProjectChange} disabled={projectsUnavailable}>
          <option value="">Daemon default</option>
          {#each projects as project (project.id)}
            <option value={project.id}>{project.label}</option>
          {/each}
        </select>
        {#if projectsUnavailable}<small>Projects are unavailable on this host.</small>{/if}
      </label>

      <label>
        <span>Branch <small>optional</small></span>
        <input
          bind:value={branch}
          disabled={selectedProject.length === 0}
          placeholder={selectedProject.length === 0 ? "Select a project first" : "feature/my-branch"}
          autocomplete="off"
        />
      </label>

      <label class="full-width">
        <span>Name <small>optional</small></span>
        <input bind:value={sessionName} autocomplete="off" placeholder="A short name for this session" />
      </label>
    </div>

    {#if open && providedTerminalSize === undefined}
      <div class="geometry-probe" aria-hidden="true">
        <TerminalSizeProbe onchange={(size) => { measuredTerminalSize = size; }} />
      </div>
    {/if}

    <footer>
      <button type="button" onclick={onclose}>Cancel</button>
      <button
        class="button-primary"
        type="submit"
        disabled={submitting || launchTerminalSize === undefined || selectedHost.length === 0 || selectedAgent.length === 0}
      >
        {submitting ? "Creating…" : "Create and attach"}
      </button>
    </footer>
  </form>
</dialog>

<style>
  .new-session-dialog {
    width: min(42rem, calc(100vw - 2rem));
    max-height: calc(100vh - 2rem);
    overflow: auto;
  }

  .dialog-heading,
  footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }

  .dialog-heading {
    margin-bottom: 1.25rem;
  }

  h2,
  p {
    margin-bottom: 0;
  }

  p,
  small {
    color: var(--muted);
  }

  .icon-button {
    min-width: 2.25rem;
    padding: 0;
    font-size: 1.35rem;
  }

  .form-grid {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 1rem;
  }

  label {
    display: grid;
    min-width: 0;
    gap: 0.4rem;
    font-weight: 650;
  }

  label small {
    font-weight: 400;
  }

  input,
  select {
    width: 100%;
    min-height: 2.6rem;
    padding: 0.5rem 0.65rem;
    border: 1px solid var(--border);
    border-radius: 0.45rem;
    color: #f3f6fc;
    background: #0b1018;
  }

  .full-width {
    grid-column: 1 / -1;
  }

  footer {
    justify-content: flex-end;
    margin-top: 1.5rem;
  }

  .geometry-probe {
    position: fixed;
    z-index: -1;
    width: min(70rem, calc(100vw - 22rem));
    min-width: 20rem;
    visibility: hidden;
    pointer-events: none;
  }

  @media (max-width: 620px) {
    .form-grid {
      grid-template-columns: 1fr;
    }

    .full-width {
      grid-column: auto;
    }
  }
</style>
