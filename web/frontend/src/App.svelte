<script lang="ts">
  import { onDestroy, onMount, tick } from "svelte";
  import CommandPalette from "./components/CommandPalette.svelte";
  import InboxDrawer from "./components/InboxDrawer.svelte";
  import NewSessionDialog from "./components/NewSessionDialog.svelte";
  import ProjectsScreen from "./components/ProjectsScreen.svelte";
  import SessionInspector from "./components/SessionInspector.svelte";
  import ToastRegion from "./components/ToastRegion.svelte";
  import WorkspaceShell from "./components/WorkspaceShell.svelte";
  import {
    createHistoryRouter,
    getBrowserWorkspace,
  } from "./lib";
  import { hostResourceKey } from "@pohunek/client-core";
  import {
    installGlobalKeybindings,
    type AppShortcut,
  } from "./lib/keybindings";
  import {
    loadUiState,
    saveUiState,
  } from "./lib/ui-state";

  const browserWorkspace = getBrowserWorkspace();
  const workspace = browserWorkspace.workspace;
  const stores = browserWorkspace.stores;
  const router = createHistoryRouter();
  const current = router.current;
  const hosts = stores.hosts;
  const sessions = stores.sessions;
  const notifications = stores.notifications;
  const initialUiState = loadUiState();

  let selectedHost: string | undefined = $state(initialUiState.selectedSession?.host);
  let selectedSessionId: string | undefined = $state(initialUiState.selectedSession?.sessionId);
  let sidebarCollapsed = $state(initialUiState.sidebarCollapsed);
  let commandPaletteOpen = $state(false);
  let validatingPersistedSelection = $state(initialUiState.selectedSession !== undefined);

  $effect((): void => {
    const route = $current;
    if (route.kind === "session" || route.kind === "terminal") {
      validatingPersistedSelection = false;
      selectedHost = route.host;
      selectedSessionId = route.sessionId;
      persistUiState();
    }
  });

  $effect((): void => {
    if (!validatingPersistedSelection || selectedHost === undefined || selectedSessionId === undefined) {
      return;
    }
    if ($sessions[hostResourceKey(selectedHost, selectedSessionId)] !== undefined) {
      validatingPersistedSelection = false;
      return;
    }

    const selectedHostSnapshot = $hosts[selectedHost];
    const hostEntries = Object.values($hosts);
    const selectedHostSettled = selectedHostSnapshot !== undefined
      && selectedHostSnapshot.connection.kind !== "connecting";
    const discoverySettledWithoutSelectedHost = selectedHostSnapshot === undefined
      && hostEntries.length > 0
      && hostEntries.every((host) => host.connection.kind !== "connecting");
    if (!selectedHostSettled && !discoverySettledWithoutSelectedHost) {
      return;
    }

    validatingPersistedSelection = false;
    selectedHost = undefined;
    selectedSessionId = undefined;
    persistUiState();
  });

  onMount((): (() => void) => installGlobalKeybindings(handleShortcut));

  onDestroy((): void => {
    router.close();
    void workspace.close();
  });

  function selectSession(host: string, sessionId: string): void {
    validatingPersistedSelection = false;
    selectedHost = host;
    selectedSessionId = sessionId;
    persistUiState();
    router.navigate({ kind: "terminal", host, sessionId });
  }

  function openNewSession(): void {
    commandPaletteOpen = false;
    router.navigate(
      selectedHost === undefined
        ? { kind: "new-session" }
        : { kind: "new-session", host: selectedHost },
    );
  }

  function openInbox(): void {
    commandPaletteOpen = false;
    router.navigate({ kind: "inbox" });
  }

  function openProjects(): void {
    const connectedHost = selectedHost ?? Object.values($hosts).find((entry) => entry.connection.kind === "connected")?.host;
    if (connectedHost === undefined) {
      return;
    }
    router.navigate({ kind: "projects", host: connectedHost });
  }

  function openProject(host: string, reference: string): void {
    router.navigate({ kind: "project", host, reference });
  }

  function selectProjectHost(host: string): void {
    router.navigate({ kind: "projects", host });
  }

  function backToProjects(): void {
    if ($current.kind === "project") {
      router.navigate({ kind: "projects", host: $current.host });
    }
  }

  function backToWorkspace(): void {
    router.navigate({ kind: "workspace" });
  }

  function openDetails(host: string, sessionId: string): void {
    router.navigate({ kind: "session", host, sessionId });
  }

  function closeOverlay(): void {
    commandPaletteOpen = false;
    if (selectedHost === undefined || selectedSessionId === undefined) {
      router.navigate({ kind: "workspace" });
      return;
    }
    router.navigate({ kind: "terminal", host: selectedHost, sessionId: selectedSessionId });
  }

  function closeTerminal(): void {
    validatingPersistedSelection = false;
    selectedHost = undefined;
    selectedSessionId = undefined;
    persistUiState();
    router.navigate({ kind: "workspace" });
  }

  function toggleSidebar(): void {
    sidebarCollapsed = !sidebarCollapsed;
    persistUiState();
  }

  function openCommandPalette(): void {
    commandPaletteOpen = true;
  }

  function openNextBlockedSession(): void {
    const blocked = Object.values($sessions)
      .filter((entry) => entry.session.activity === "blocked")
      .sort((left, right) => right.session.updated_at.localeCompare(left.session.updated_at));
    if (blocked.length === 0) {
      return;
    }
    const currentIndex = blocked.findIndex(
      (entry) => entry.host === selectedHost && entry.session.id === selectedSessionId,
    );
    const next = blocked[(currentIndex + 1) % blocked.length];
    if (next !== undefined) {
      selectSession(next.host, next.session.id);
    }
  }

  function handleShortcut(shortcut: AppShortcut): void {
    const routeOverlayOpen = $current.kind !== "workspace" && $current.kind !== "terminal";
    if (commandPaletteOpen && shortcut !== "command-palette" && shortcut !== "dismiss") {
      return;
    }
    if (routeOverlayOpen && shortcut !== "dismiss") {
      return;
    }
    switch (shortcut) {
      case "command-palette":
        commandPaletteOpen = !commandPaletteOpen;
        break;
      case "toggle-sidebar":
        toggleSidebar();
        break;
      case "new-session":
        openNewSession();
        break;
      case "inbox":
        openInbox();
        break;
      case "next-blocked":
        openNextBlockedSession();
        break;
      case "focus-search":
        void focusSessionSearch();
        break;
      case "next-item":
        focusRelativeSession(1);
        break;
      case "previous-item":
        focusRelativeSession(-1);
        break;
      case "activate-item":
        activateFocusedSession();
        break;
      case "dismiss":
        if (commandPaletteOpen) {
          commandPaletteOpen = false;
        } else if ($current.kind !== "workspace" && $current.kind !== "terminal") {
          closeOverlay();
        }
        break;
    }
  }

  async function focusSessionSearch(): Promise<void> {
    if (sidebarCollapsed) {
      sidebarCollapsed = false;
      persistUiState();
      await tick();
    }
    document.querySelector<HTMLInputElement>("[data-session-search]")?.focus();
  }

  function focusRelativeSession(direction: 1 | -1): void {
    const items = Array.from(document.querySelectorAll<HTMLButtonElement>('[data-testid="session-row"]'));
    if (items.length === 0) {
      return;
    }
    const focusedIndex = items.findIndex((item) => item === document.activeElement);
    const selectedIndex = items.findIndex(
      (item) => item.dataset.host === selectedHost && item.dataset.sessionId === selectedSessionId,
    );
    const origin = focusedIndex >= 0 ? focusedIndex : selectedIndex;
    const nextIndex = origin < 0
      ? direction === 1 ? 0 : items.length - 1
      : (origin + direction + items.length) % items.length;
    const next = items[nextIndex];
    next?.focus();
  }

  function activateFocusedSession(): void {
    const active = document.activeElement;
    if (active instanceof HTMLButtonElement && active.matches('[data-testid="session-row"]')) {
      active.click();
      return;
    }
    document.querySelector<HTMLButtonElement>('[data-testid="session-row"][aria-current="page"]')?.click();
  }

  function persistUiState(): void {
    if (selectedHost === undefined || selectedSessionId === undefined) {
      saveUiState({ sidebarCollapsed });
      return;
    }
    saveUiState({
      sidebarCollapsed,
      selectedSession: { host: selectedHost, sessionId: selectedSessionId },
    });
  }
</script>

<div class="app-shell">
  <div
    class="workspace-layer"
    inert={$current.kind !== "workspace" && $current.kind !== "terminal" || commandPaletteOpen}
  >
    <WorkspaceShell
      {workspace}
      {hosts}
      {sessions}
      unreadCount={$notifications.unreadCount}
      {selectedHost}
      {selectedSessionId}
      {sidebarCollapsed}
      onselect={selectSession}
      onnewsession={openNewSession}
      onprojects={openProjects}
      oninbox={openInbox}
      ondetails={openDetails}
      onclose={closeTerminal}
      ontogglesidebar={toggleSidebar}
      oncommandpalette={openCommandPalette}
      onblocked={openNextBlockedSession}
    />
  </div>

  {#if $current.kind === "new-session"}
    <NewSessionDialog
      open
      {workspace}
      {hosts}
      selectedHost={$current.host ?? selectedHost}
      onclose={closeOverlay}
      oncreated={selectSession}
    />
  {/if}

  {#if $current.kind === "projects" || $current.kind === "project"}
    <ProjectsScreen
      {workspace}
      {hosts}
      host={$current.host}
      reference={$current.kind === "project" ? $current.reference : undefined}
      onopenproject={openProject}
      onopensession={selectSession}
      onselecthost={selectProjectHost}
      onworkspace={backToWorkspace}
      onbacktoprojects={backToProjects}
    />
  {/if}

  <InboxDrawer
    open={$current.kind === "inbox"}
    {workspace}
    {notifications}
    onclose={closeOverlay}
    onopensession={selectSession}
  />

  {#if $current.kind === "session"}
    <SessionInspector
      open
      {workspace}
      {sessions}
      host={$current.host}
      sessionId={$current.sessionId}
      onclose={closeOverlay}
      onopenterminal={selectSession}
    />
  {/if}

  <CommandPalette
    open={commandPaletteOpen}
    {sessions}
    onclose={() => { commandPaletteOpen = false; }}
    onnewsession={openNewSession}
    oninbox={openInbox}
    onblocked={openNextBlockedSession}
    onopensession={selectSession}
  />

  <ToastRegion />
</div>
