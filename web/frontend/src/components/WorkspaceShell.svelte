<script lang="ts">
  import {
    hostResourceKey,
    type HostsSnapshot,
    type SessionsSnapshot,
    type Workspace,
  } from "@pohunek/client-core";
  import type { Readable } from "svelte/store";
  import SessionMain from "./SessionMain.svelte";
  import SessionRail from "./SessionRail.svelte";

  interface Props {
    workspace: Workspace;
    hosts: Readable<HostsSnapshot>;
    sessions: Readable<SessionsSnapshot>;
    unreadCount: number;
    selectedHost?: string | undefined;
    selectedSessionId?: string | undefined;
    sidebarCollapsed?: boolean;
    onselect: (host: string, sessionId: string) => void;
    onnewsession: () => void;
    oninbox: () => void;
    ondetails: (host: string, sessionId: string) => void;
    onclose: () => void;
    ontogglesidebar: () => void;
    oncommandpalette: () => void;
    onblocked: () => void;
  }

  let {
    workspace,
    hosts,
    sessions,
    unreadCount,
    selectedHost,
    selectedSessionId,
    sidebarCollapsed = false,
    onselect,
    onnewsession,
    oninbox,
    ondetails,
    onclose,
    ontogglesidebar,
    oncommandpalette,
    onblocked,
  }: Props = $props();

  const selectedEntry = $derived(
    selectedHost === undefined || selectedSessionId === undefined
      ? undefined
      : $sessions[hostResourceKey(selectedHost, selectedSessionId)],
  );
  const blockedCount = $derived(
    Object.values($sessions).filter((entry) => entry.session.activity === "blocked").length,
  );
</script>

<div class:workspace-sidebar-collapsed={sidebarCollapsed} class="workspace-shell">
  <header class="workspace-topbar">
    <div class="workspace-brand">
      <button
        class="icon-button"
        type="button"
        onclick={ontogglesidebar}
        aria-label={sidebarCollapsed ? "Show session rail" : "Hide session rail"}
        title={sidebarCollapsed ? "Show session rail" : "Hide session rail"}
      >
        <span aria-hidden="true">☰</span>
      </button>
      <strong>Pohunek</strong>
    </div>
    <div class="workspace-actions">
      {#if blockedCount > 0}
        <button class="attention-button" type="button" onclick={onblocked}>
          <span class="activity-dot activity-blocked" aria-hidden="true"></span>
          {blockedCount} blocked
        </button>
      {/if}
      <button class="command-button" type="button" onclick={oncommandpalette}>
        Search or jump
        <kbd>Ctrl K</kbd>
      </button>
      <button type="button" onclick={oninbox}>
        Inbox
        {#if unreadCount > 0}<span class="unread-count" data-testid="unread-count">{unreadCount}</span>{/if}
      </button>
      <button class="button-primary" type="button" onclick={onnewsession}>New session</button>
    </div>
  </header>

  <SessionRail
    {hosts}
    {sessions}
    selectedHost={selectedEntry?.host ?? selectedHost}
    selectedSessionId={selectedEntry?.session.id ?? selectedSessionId}
    collapsed={sidebarCollapsed}
    {onselect}
    {onnewsession}
  />
  <SessionMain
    {workspace}
    entry={selectedEntry}
    {ondetails}
    {onclose}
    {onnewsession}
  />
</div>
