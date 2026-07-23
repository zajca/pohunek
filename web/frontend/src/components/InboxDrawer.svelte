<script lang="ts">
  import type { NotificationsSnapshot, Workspace } from "@pohunek/client-core";
  import { SvelteSet } from "svelte/reactivity";
  import { onDestroy, tick } from "svelte";
  import type { Readable } from "svelte/store";
  import { addErrorToast } from "../lib";

  interface Props {
    open: boolean;
    workspace: Workspace;
    notifications: Readable<NotificationsSnapshot>;
    onclose: () => void;
    onopensession: (host: string, sessionId: string) => void;
  }

  type NotificationStatus = Parameters<Workspace["actions"]["notificationUpdate"]>[1]["status"];

  let { open, workspace, notifications, onclose, onopensession }: Props = $props();
  let filter: "unread" | "all" = $state("unread");
  let drawer = $state<HTMLDivElement>();
  let closeButton = $state<HTMLButtonElement>();
  let previouslyFocused: HTMLElement | undefined;
  let focusOwned = false;
  const pending = new SvelteSet<string>();

  const records = $derived(
    Object.values($notifications.records)
      .filter((entry) => filter === "all" || entry.notification.status === "unread")
      .sort((left, right) => {
        const unreadOrder = Number(right.notification.status === "unread") - Number(left.notification.status === "unread");
        return unreadOrder !== 0
          ? unreadOrder
          : right.notification.created_at.localeCompare(left.notification.created_at);
      }),
  );

  $effect((): void => {
    if (open && !focusOwned) {
      focusOwned = true;
      previouslyFocused = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
      void tick().then((): void => {
        if (open) {
          closeButton?.focus();
        }
      });
    } else if (!open && focusOwned) {
      restoreFocus();
    }
  });

  onDestroy(restoreFocus);

  function update(host: string, id: string, status: NotificationStatus): void {
    const key = `${host}:${id}`;
    pending.add(key);
    void workspace.actions.notificationUpdate(host, { id, status })
      .catch((error: unknown): void => {
        addErrorToast(error);
      })
      .finally((): void => {
        pending.delete(key);
      });
  }

  function openSession(host: string, id: string, sessionId: string, status: NotificationStatus): void {
    if (status === "unread") {
      update(host, id, "read");
    }
    onopensession(host, sessionId);
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
  <button class="drawer-backdrop" type="button" tabindex="-1" aria-label="Close inbox" onclick={onclose}></button>
  <div
    bind:this={drawer}
    class="drawer"
    role="dialog"
    aria-modal="true"
    aria-labelledby="inbox-title"
    tabindex="-1"
    onkeydown={trapDrawerFocus}
  >
    <header class="drawer-heading">
      <div>
        <h2 id="inbox-title">Inbox</h2>
        <p>{$notifications.unreadCount} unread</p>
      </div>
      <button bind:this={closeButton} class="icon-button" type="button" aria-label="Close inbox" onclick={onclose}>×</button>
    </header>

    <div class="filter-tabs" aria-label="Notification filter">
      <button class:active={filter === "unread"} type="button" onclick={() => { filter = "unread"; }}>
        Unread
      </button>
      <button class:active={filter === "all"} type="button" onclick={() => { filter = "all"; }}>
        All
      </button>
    </div>

    {#if records.length === 0}
      <p class="empty">{filter === "unread" ? "You're all caught up." : "The inbox is empty."}</p>
    {:else}
      <ul class="notification-list">
        {#each records as entry (`${entry.host}:${entry.notification.id}`)}
          <li
            class:unread={entry.notification.status === "unread"}
            class:attention={entry.notification.severity === "warning"
              || entry.notification.severity === "error"
              || entry.notification.severity === "action_required"}
            data-testid="notification-card"
            data-host={entry.host}
            data-notification-id={entry.notification.id}
          >
            <div class="notification-heading">
              <div>
                <h3>{entry.notification.title}</h3>
                <span>{entry.host} · {entry.notification.kind}</span>
              </div>
              {#if entry.notification.status === "unread"}<span class="unread-dot">New</span>{/if}
            </div>
            <p>{entry.notification.body}</p>
            <time datetime={entry.notification.created_at}>{entry.notification.created_at}</time>
            <div class="notification-actions">
              {#if entry.notification.session_id !== undefined}
                <button
                  class="button-primary"
                  type="button"
                  disabled={pending.has(`${entry.host}:${entry.notification.id}`)}
                  onclick={() => openSession(
                    entry.host,
                    entry.notification.id,
                    entry.notification.session_id!,
                    entry.notification.status,
                  )}
                >
                  Open session
                </button>
              {/if}
              {#if entry.notification.status === "unread" || entry.notification.status === "read"}
                <button
                  type="button"
                  disabled={pending.has(`${entry.host}:${entry.notification.id}`)}
                  onclick={() => update(entry.host, entry.notification.id, "acknowledged")}
                >
                  Acknowledge
                </button>
              {/if}
              {#if entry.notification.status !== "archived" && entry.notification.status !== "deleted"}
                <details class="more-actions">
                  <summary aria-label="More notification actions">•••</summary>
                  <button
                    type="button"
                    disabled={pending.has(`${entry.host}:${entry.notification.id}`)}
                    onclick={() => update(entry.host, entry.notification.id, "archived")}
                  >
                    Archive
                  </button>
                </details>
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
{/if}

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
    width: min(31rem, 100vw);
    overflow: auto;
    padding: 1.25rem;
    border-left: 1px solid var(--border);
    background: var(--surface-raised);
    box-shadow: -1rem 0 3rem rgb(0 0 0 / 35%);
  }

  .drawer-heading,
  .notification-heading,
  .notification-actions {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 0.75rem;
  }

  h2,
  h3,
  p {
    margin-bottom: 0;
  }

  .drawer-heading p,
  .notification-heading span,
  time {
    color: var(--muted);
    font-size: 0.8rem;
  }

  .icon-button {
    min-width: 2.25rem;
    padding: 0;
    font-size: 1.35rem;
  }

  .filter-tabs {
    display: flex;
    gap: 0.4rem;
    margin: 1.25rem 0 1rem;
  }

  .filter-tabs button {
    min-height: 2rem;
    padding: 0.3rem 0.75rem;
  }

  .filter-tabs .active {
    border-color: var(--accent);
    color: #fff;
    background: #24426c;
  }

  .notification-list {
    display: grid;
    gap: 0.65rem;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  li {
    padding: 0.9rem;
    border: 1px solid var(--border);
    border-left: 3px solid transparent;
    border-radius: 0.55rem;
    background: var(--surface);
  }

  li.unread {
    border-left-color: var(--accent);
  }

  li.attention {
    border-left-color: var(--danger);
  }

  li > p {
    margin-top: 0.7rem;
    color: #d4dbe7;
    line-height: 1.45;
  }

  .unread-dot {
    color: var(--accent-strong) !important;
    font-weight: 700;
  }

  .notification-actions {
    justify-content: flex-start;
    margin-top: 0.75rem;
  }

  .notification-actions button {
    min-height: 2rem;
    padding: 0.35rem 0.65rem;
    font-size: 0.8rem;
  }

  .more-actions {
    position: relative;
  }

  .more-actions summary {
    min-width: 2rem;
    padding: 0.35rem;
    cursor: pointer;
    list-style: none;
    text-align: center;
  }

  .more-actions button {
    position: absolute;
    z-index: 1;
    top: 2rem;
    right: 0;
  }

  .empty {
    padding: 3rem 1rem;
    color: var(--muted);
    text-align: center;
  }
</style>
