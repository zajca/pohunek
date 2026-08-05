<script lang="ts">
  import type {
    HostedSession,
    Workspace,
  } from "@pohunek/client-core";
  import { hasAttachableSessionAgentBases, sessionAgentLabel } from "../lib";
  import EmbeddedTerminal from "./EmbeddedTerminal.svelte";
  import SessionLifecycleActions from "./SessionLifecycleActions.svelte";

  interface Props {
    workspace: Workspace;
    entry: HostedSession | undefined;
    ondetails: (host: string, sessionId: string) => void;
    onclose: () => void;
    onnewsession: () => void;
    onselect: (host: string, sessionId: string) => void;
  }

  let { workspace, entry, ondetails, onclose, onnewsession, onselect }: Props = $props();

  const canAttach = $derived(
    entry?.session.state === "running"
      && entry.session.external !== true
      && hasAttachableSessionAgentBases(entry.session.agent_base, entry.session.active_agent_base),
  );

  function openDetails(): void {
    if (entry !== undefined) {
      ondetails(entry.host, entry.session.id);
    }
  }

  function sessionTitle(selected: HostedSession): string {
    return selected.session.name ?? selected.session.branch ?? selected.session.id;
  }

  function agentLabel(selected: HostedSession): string {
    const session = selected.session;
    return session.active_agent !== undefined
      ? sessionAgentLabel(session.active_agent, session.active_agent_base)
      : sessionAgentLabel(session.agent, session.agent_base);
  }

  function summaryTitle(selected: HostedSession): string {
    if (selected.session.external === true) {
      return "Observe-only session";
    }
    switch (selected.session.state) {
      case "starting":
        return "Session is starting";
      case "stopped":
        return "Session is stopped";
      case "done":
        return "Session completed";
      case "failed":
        return "Session failed";
      case "running":
        return "Terminal unavailable";
    }
  }

  function summaryBody(selected: HostedSession): string {
    if (selected.session.external === true) {
      return "This process is observed by Pohunek but does not expose an attachable PTY.";
    }
    switch (selected.session.state) {
      case "starting":
        return "The daemon is preparing the agent process. The terminal will become available when it is running.";
      case "stopped":
        return "The agent process is no longer running. Open details to inspect its metadata.";
      case "done":
        return "The agent process exited successfully. Its metadata remains available for reference.";
      case "failed":
        return selected.session.exit_code === undefined
          ? "The agent process exited with a failure. Open details for diagnostic context."
          : `The agent process exited with code ${selected.session.exit_code}.`;
      case "running":
        return "This running session does not expose an attachable terminal.";
    }
  }
</script>

<main class="session-main">
  {#if entry === undefined}
    <section class="workspace-welcome" aria-labelledby="workspace-welcome-heading">
      <span class="welcome-mark" aria-hidden="true">P</span>
      <h2 id="workspace-welcome-heading">Choose a session</h2>
      <p>Select an agent session from the rail to attach without leaving the workspace.</p>
      <button class="button-primary" type="button" onclick={onnewsession}>Start a new session</button>
    </section>
  {:else}
    <header class="session-toolbar">
      <div class="session-identity">
        <div class="session-title-line">
          {#if entry.session.activity !== undefined}
            <span class={`activity-dot activity-${entry.session.activity}`} aria-hidden="true"></span>
          {:else}
            <span class="activity-dot activity-unknown" aria-hidden="true"></span>
          {/if}
          <h2>{sessionTitle(entry)}</h2>
          <span class="lifecycle-label">{entry.session.state}</span>
        </div>
        <p>
          <span>{entry.session.project_label ?? entry.session.repo ?? "No project"}</span>
          {#if entry.session.branch !== undefined}<span> / {entry.session.branch}</span>{/if}
          <span> · {entry.host}</span>
          <span> · {agentLabel(entry)}</span>
        </p>
      </div>
      <div class="session-toolbar-actions">
        <button type="button" onclick={openDetails}>Details</button>
        {#if entry.session.external !== true}
          <SessionLifecycleActions
            {workspace}
            {entry}
            onfork={onselect}
            onremove={onclose}
          />
        {/if}
        {#if canAttach}<button type="button" onclick={onclose}>Close terminal</button>{/if}
      </div>
    </header>

    {#if canAttach}
      {#key `${entry.host}:${entry.session.id}`}
        <EmbeddedTerminal workspace={workspace} host={entry.host} sessionId={entry.session.id} />
      {/key}
    {:else}
      <section class="session-summary" data-testid="session-summary">
        <span class={`summary-icon summary-${entry.session.state}`} aria-hidden="true"></span>
        <h3>{summaryTitle(entry)}</h3>
        <p>{summaryBody(entry)}</p>
        <dl>
          <div><dt>Agent</dt><dd>{agentLabel(entry)}</dd></div>
          <div><dt>Host</dt><dd>{entry.host}</dd></div>
          <div><dt>Working directory</dt><dd>{entry.session.cwd}</dd></div>
          {#if entry.session.activity !== undefined}
            <div><dt>Activity</dt><dd>{entry.session.activity}</dd></div>
          {/if}
        </dl>
        <button type="button" onclick={openDetails}>Open session details</button>
      </section>
    {/if}
  {/if}
</main>
