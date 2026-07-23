<script lang="ts">
  import type { HostsSnapshot, Workspace } from "@pohunek/client-core";
  import { onDestroy } from "svelte";
  import type { Readable } from "svelte/store";
  import { addErrorToast, addToast } from "../lib";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import FormDialog from "./FormDialog.svelte";

  interface Props {
    workspace: Workspace;
    hosts: Readable<HostsSnapshot>;
    host: string;
    reference?: string | undefined;
    onopenproject: (host: string, reference: string) => void;
    onopensession: (host: string, sessionId: string) => void;
    onselecthost: (host: string) => void;
    onworkspace: () => void;
    onbacktoprojects: () => void;
  }

  type Projects = Awaited<ReturnType<Workspace["actions"]["projectList"]>>;
  type Detail = Awaited<ReturnType<Workspace["actions"]["projectShow"]>>;

  let {
    workspace,
    hosts,
    host,
    reference,
    onopenproject,
    onopensession,
    onselecthost,
    onworkspace,
    onbacktoprojects,
  }: Props = $props();
  let projects: Projects = $state([]);
  let detail: Detail | undefined = $state();
  let loading = $state(false);
  let failed = $state(false);
  let addOpen = $state(false);
  let renameOpen = $state(false);
  let removeOpen = $state(false);
  let removeWorktreePath: string | undefined = $state();
  let path = $state("");
  let name = $state("");
  let baseBranch = $state("");
  let pruneWorktrees = $state(false);
  let pending = $state(false);
  let generation = 0;

  const connected = $derived($hosts[host]?.connection.kind === "connected");
  const connectedHosts = $derived(
    Object.values($hosts)
      .filter((candidate) => candidate.connection.kind === "connected")
      .sort((left, right) => left.host.localeCompare(right.host)),
  );

  $effect((): void => {
    void load();
  });

  onDestroy((): void => {
    generation += 1;
  });

  async function load(): Promise<void> {
    const current = generation + 1;
    generation = current;
    loading = true;
    failed = false;
    try {
      const listed = await workspace.actions.projectList(host, {});
      const shown = reference === undefined
        ? undefined
        : await workspace.actions.projectShow(host, { reference });
      if (current !== generation) {
        return;
      }
      projects = listed;
      detail = shown;
    } catch (error: unknown) {
      if (current === generation) {
        failed = true;
        addErrorToast(error);
      }
    } finally {
      if (current === generation) {
        loading = false;
      }
    }
  }

  async function addProject(): Promise<void> {
    const projectLocation = path.trim();
    if (!projectLocation.startsWith("/")) {
      addToast("error", "Project path must be absolute");
      return;
    }

    pending = true;
    try {
      const project = await workspace.actions.projectAdd(host, {
        path: projectLocation,
        ...(name.trim().length === 0 ? {} : { name: name.trim() }),
        ...(baseBranch.trim().length === 0 ? {} : { base_branch: baseBranch.trim() }),
      });
      addOpen = false;
      path = "";
      name = "";
      baseBranch = "";
      addToast("success", "Project added");
      onopenproject(host, project.id);
    } catch (error: unknown) {
      addErrorToast(error);
    } finally {
      pending = false;
    }
  }

  async function renameProject(): Promise<void> {
    if (detail === undefined || name.trim().length === 0) {
      addToast("error", "Project name must not be empty");
      return;
    }

    pending = true;
    try {
      await workspace.actions.projectRename(host, {
        reference: detail.project.id,
        name: name.trim(),
      });
      renameOpen = false;
      addToast("success", "Project renamed");
      await load();
    } catch (error: unknown) {
      addErrorToast(error);
    } finally {
      pending = false;
    }
  }

  async function removeProject(): Promise<void> {
    if (detail === undefined) {
      return;
    }

    pending = true;
    try {
      await workspace.actions.projectRemove(host, {
        reference: detail.project.id,
        prune_worktrees: pruneWorktrees,
      });
      addToast("success", "Project removed");
      onbacktoprojects();
    } catch (error: unknown) {
      addErrorToast(error);
    } finally {
      pending = false;
      removeOpen = false;
    }
  }

  async function removeWorktree(): Promise<void> {
    if (removeWorktreePath === undefined) {
      return;
    }

    pending = true;
    try {
      await workspace.actions.worktreeRemove(host, { path: removeWorktreePath });
      addToast("success", "Worktree removed");
      removeWorktreePath = undefined;
      await load();
    } catch (error: unknown) {
      addErrorToast(error);
    } finally {
      pending = false;
    }
  }

  function openRename(): void {
    name = detail?.project.label ?? "";
    renameOpen = true;
  }

  function closeFormDialog(): void {
    addOpen = false;
    renameOpen = false;
  }
</script>

<main class="projects" data-testid="projects-screen">
  <header>
    <div>
      <h1>{reference === undefined ? "Projects" : detail?.project.label ?? "Project"}</h1>
      <p>{host}</p>
    </div>
    <div class="actions">
      <label class="host-selector">
        <span>Host</span>
        <select
          aria-label="Project host"
          value={host}
          onchange={(event) => onselecthost(event.currentTarget.value)}
        >
          {#each connectedHosts as candidate (candidate.host)}
            <option value={candidate.host}>{candidate.host}</option>
          {/each}
        </select>
      </label>
      <button type="button" onclick={onworkspace}>Back to workspace</button>
      <button type="button" onclick={() => void load()} disabled={!connected || loading}>Refresh</button>
      {#if reference === undefined}
        <button
          class="button-primary"
          type="button"
          disabled={!connected}
          onclick={() => { addOpen = true; }}
        >
          Add project
        </button>
      {:else}
        <button type="button" onclick={onbacktoprojects}>All projects</button>
      {/if}
    </div>
  </header>

  {#if !connected}
    <p class="state-message">This host is unavailable. Project changes are disabled.</p>
  {:else if loading && (reference === undefined ? projects.length === 0 : detail === undefined)}
    <p class="state-message">Loading projects…</p>
  {:else if failed}
    <section class="error-panel">
      <p>Projects could not be loaded.</p>
      <button type="button" onclick={() => void load()}>Retry</button>
    </section>
  {:else if reference === undefined}
    <div class="project-list">
      {#each projects as project (project.id)}
        <button type="button" class="project-card" onclick={() => onopenproject(host, project.id)}>
          <strong>{project.label}</strong>
          <span>{project.repo_root}</span>
          <small>{project.default_base_branch ?? "Repository HEAD"}</small>
        </button>
      {:else}
        <p class="state-message">No projects registered on this host.</p>
      {/each}
    </div>
  {:else if detail !== undefined}
    <section class="project-detail">
      <div class="detail-actions">
        <button type="button" onclick={openRename}>Rename</button>
        <button
          class="button-danger"
          type="button"
          onclick={() => {
            pruneWorktrees = false;
            removeOpen = true;
          }}
        >
          Remove project
        </button>
      </div>
      <dl>
        <div><dt>Repository root</dt><dd>{detail.project.repo_root}</dd></div>
        <div><dt>Base branch</dt><dd>{detail.project.default_base_branch ?? "Repository HEAD"}</dd></div>
        <div><dt>Git common dir</dt><dd>{detail.project.git_common_dir}</dd></div>
      </dl>
      <h2>Worktrees</h2>
      <div class="worktrees">
        {#each detail.worktrees as worktree (worktree.path)}
          <article>
            <strong>{worktree.path}</strong>
            <span>{worktree.branch ?? "Detached HEAD"} · {worktree.head ?? "HEAD unavailable"}</span>
            <small>{worktree.owned ? "Pohunek-owned" : "External"}{worktree.locked ? " · Locked" : ""}</small>
            {#if worktree.session_id !== undefined}
              {@const linkedSessionId = worktree.session_id}
              <button type="button" onclick={() => onopensession(host, linkedSessionId)}>
                Open live session
              </button>
            {:else if worktree.owned}
              <button
                class="button-danger"
                type="button"
                onclick={() => { removeWorktreePath = worktree.path; }}
              >
                Remove worktree
              </button>
            {/if}
          </article>
        {:else}
          <p class="state-message">No live worktrees found.</p>
        {/each}
      </div>
    </section>
  {/if}
</main>

<FormDialog
  open={addOpen || renameOpen}
  title={addOpen ? "Add project" : "Rename project"}
  submitting={pending}
  onclose={closeFormDialog}
  onsubmit={() => {
    if (addOpen) {
      void addProject();
    } else {
      void renameProject();
    }
  }}
>
  {#if addOpen}
    <label>Absolute path<input bind:value={path} placeholder="/srv/project" /></label>
    <label>Display name (optional)<input bind:value={name} /></label>
    <label>Default base branch (optional)<input bind:value={baseBranch} /></label>
  {:else}
    <label>Display name<input bind:value={name} /></label>
  {/if}
</FormDialog>

<ConfirmDialog
  open={removeOpen}
  title="Remove this project?"
  message="The project record will be removed. Only Pohunek-owned worktrees can be pruned."
  confirmLabel="Remove project"
  onconfirm={() => void removeProject()}
  oncancel={() => { removeOpen = false; }}
>
  <label class="prune">
    <input type="checkbox" bind:checked={pruneWorktrees} />
    Also prune Pohunek-owned worktrees
  </label>
</ConfirmDialog>

<ConfirmDialog
  open={removeWorktreePath !== undefined}
  title="Remove this worktree?"
  message="This permanently removes the Pohunek-owned worktree. The daemon will reject a protected or active worktree."
  confirmLabel="Remove worktree"
  onconfirm={() => void removeWorktree()}
  oncancel={() => { removeWorktreePath = undefined; }}
/>

<style>
  .projects {
    position: fixed;
    z-index: 30;
    inset: 0;
    overflow: auto;
    padding: 2rem;
    background: var(--surface);
  }

  header,
  .actions,
  .detail-actions {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
  }

  h1,
  h2,
  p {
    margin: 0;
  }

  header p,
  small,
  span {
    color: var(--muted);
  }

  .host-selector {
    display: grid;
    gap: 0.2rem;
    font-size: 0.8rem;
  }

  .project-list,
  .worktrees {
    display: grid;
    gap: 0.75rem;
    margin-top: 1.5rem;
  }

  .project-card,
  .worktrees article {
    display: grid;
    gap: 0.3rem;
    padding: 1rem;
    text-align: left;
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    background: var(--surface-raised);
  }

  .project-card:hover {
    border-color: var(--accent);
  }

  .project-detail {
    display: grid;
    gap: 1.25rem;
    margin-top: 1.5rem;
  }

  dl {
    display: grid;
    gap: 0.5rem;
  }

  dl div {
    display: grid;
    grid-template-columns: 10rem 1fr;
    gap: 1rem;
  }

  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }

  .prune {
    margin-top: 0.75rem;
  }

  .prune input {
    width: auto;
  }

  @media (max-width: 768px) {
    .projects {
      padding: 1rem;
    }

    header {
      align-items: flex-start;
    }

    .actions {
      flex-wrap: wrap;
      justify-content: flex-end;
    }
  }
</style>
