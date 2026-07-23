<script lang="ts">
  import {
    hostResourceKey,
    type HostsSnapshot,
    type SessionsSnapshot,
    type Workspace,
  } from "@pohunek/client-core";
  import { flushSync, onMount, tick } from "svelte";
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
    onprojects: () => void;
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
    onprojects,
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

  const MOBILE_SHELL_QUERY = "(max-width: 768px), (pointer: coarse) and (max-height: 500px)";
  const FOCUSABLE_SELECTOR = [
    "a[href]",
    "button:not([disabled])",
    "input:not([disabled])",
    "select:not([disabled])",
    "textarea:not([disabled])",
    "[tabindex]:not([tabindex='-1'])",
  ].join(",");

  let mobile = $state(false);
  let mobileRailOpen = $state(false);
  let menuButton = $state<HTMLButtonElement>();
  let mobileRailPanel = $state<HTMLDivElement>();
  let previouslyFocused: HTMLElement | undefined;
  let focusTimer: ReturnType<typeof setTimeout> | undefined;

  onMount((): (() => void) => {
    const media = window.matchMedia(MOBILE_SHELL_QUERY);
    const update = (): void => {
      mobile = media.matches;
      if (!mobile) {
        mobileRailOpen = false;
      }
    };
    update();
    media.addEventListener("change", update);
    return (): void => media.removeEventListener("change", update);
  });

  function toggleRail(): void {
    if (!mobile) {
      ontogglesidebar();
      return;
    }
    if (mobileRailOpen) {
      void closeMobileRail();
    } else {
      openMobileRail();
    }
  }

  function openMobileRail(): void {
    previouslyFocused = document.activeElement instanceof HTMLElement ? document.activeElement : menuButton;
    mobileRailOpen = true;
    flushSync();
    focusMobileRail();
    focusTimer = setTimeout(focusMobileRail, 0);
  }

  async function closeMobileRail(restoreFocus = true): Promise<void> {
    if (!mobileRailOpen) {
      return;
    }
    if (focusTimer !== undefined) {
      clearTimeout(focusTimer);
      focusTimer = undefined;
    }
    mobileRailOpen = false;
    await tick();
    await nextAnimationFrame();
    if (restoreFocus) {
      (previouslyFocused ?? menuButton)?.focus();
    }
    previouslyFocused = undefined;
  }

  function selectFromRail(host: string, sessionId: string): void {
    onselect(host, sessionId);
    if (mobile) {
      void closeMobileRail();
    }
  }

  function createFromRail(): void {
    if (mobile) {
      void closeMobileRail(false);
    }
    onnewsession();
  }

  function handleMobileRailKeydown(event: KeyboardEvent): void {
    if (!mobile || !mobileRailOpen) {
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      void closeMobileRail();
      return;
    }
    if (event.key !== "Tab") {
      return;
    }

    const focusable = focusableRailElements();
    if (focusable.length === 0) {
      event.preventDefault();
      mobileRailPanel?.focus();
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

  function focusableRailElements(): HTMLElement[] {
    return Array.from(mobileRailPanel?.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR) ?? [])
      .filter((element) => element.tabIndex >= 0
        && !element.hasAttribute("inert")
        && element.getClientRects().length > 0
        && getComputedStyle(element).visibility !== "hidden");
  }

  function focusMobileRail(): void {
    if (mobileRailOpen) {
      const close = mobileRailPanel?.querySelector<HTMLElement>("[aria-label='Close session navigation']");
      (close ?? mobileRailPanel?.querySelector<HTMLElement>("button"))?.focus();
    }
    focusTimer = undefined;
  }

  function nextAnimationFrame(): Promise<void> {
    return new Promise((resolve) => requestAnimationFrame((): void => resolve()));
  }
</script>

<div class:workspace-sidebar-collapsed={sidebarCollapsed} class="workspace-shell">
  <header class="workspace-topbar" inert={mobile && mobileRailOpen}>
    <div class="workspace-brand">
      <button
        bind:this={menuButton}
        class="icon-button"
        type="button"
        onclick={toggleRail}
        aria-controls={mobile ? "mobile-session-navigation" : undefined}
        aria-expanded={mobile ? mobileRailOpen : undefined}
        aria-label={mobile
          ? mobileRailOpen ? "Close session navigation" : "Open session navigation"
          : sidebarCollapsed ? "Show session rail" : "Hide session rail"}
        title={mobile
          ? mobileRailOpen ? "Close session navigation" : "Open session navigation"
          : sidebarCollapsed ? "Show session rail" : "Hide session rail"}
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
      <button type="button" onclick={onprojects}>Projects</button>
      <button class="button-primary" type="button" onclick={onnewsession}>New session</button>
    </div>
  </header>

  {#if mobile}
    {#if mobileRailOpen}
      <button
        class="mobile-rail-backdrop"
        type="button"
        tabindex="-1"
        aria-label="Close session navigation"
        onclick={() => void closeMobileRail()}
      ></button>
      <div
        bind:this={mobileRailPanel}
        id="mobile-session-navigation"
        class="session-rail-container mobile-rail mobile-rail-open"
        role="dialog"
        aria-modal="true"
        aria-label="Session navigation"
        tabindex="-1"
        onkeydown={handleMobileRailKeydown}
      >
        <SessionRail
          {hosts}
          {sessions}
          selectedHost={selectedEntry?.host ?? selectedHost}
          selectedSessionId={selectedEntry?.session.id ?? selectedSessionId}
          collapsed={false}
          onselect={selectFromRail}
          onnewsession={createFromRail}
          onclose={() => void closeMobileRail()}
        />
      </div>
    {/if}
  {:else}
    <div class="session-rail-container">
      <SessionRail
        {hosts}
        {sessions}
        selectedHost={selectedEntry?.host ?? selectedHost}
        selectedSessionId={selectedEntry?.session.id ?? selectedSessionId}
        collapsed={sidebarCollapsed}
        onselect={selectFromRail}
        onnewsession={createFromRail}
      />
    </div>
  {/if}
  <div class="session-main-container" inert={mobile && mobileRailOpen}>
    <SessionMain
      {workspace}
      entry={selectedEntry}
      {ondetails}
      {onclose}
      {onnewsession}
      onselect={onselect}
    />
  </div>
</div>
