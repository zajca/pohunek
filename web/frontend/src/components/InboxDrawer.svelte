<script lang="ts">
  import type { HostsSnapshot, NotificationsSnapshot, Workspace } from "@pohunek/client-core";
  import type { NotificationKindPolicy, NotificationPolicy } from "@pohunek/protocol";
  import { SvelteSet } from "svelte/reactivity";
  import { onDestroy, tick } from "svelte";
  import type { Readable } from "svelte/store";
  import { addErrorToast, LatestRequest } from "../lib";

  interface Props {
    open: boolean;
    workspace: Workspace;
    hosts: Readable<HostsSnapshot>;
    notifications: Readable<NotificationsSnapshot>;
    onclose: () => void;
    onopensession: (host: string, sessionId: string) => void;
  }

  type NotificationStatus = Parameters<Workspace["actions"]["notificationUpdate"]>[1]["status"];

  const NOTIFICATION_KINDS = [
    "agent_blocked",
    "approval_required",
    "turn_completed",
    "session_finished",
    "error",
    "system",
  ] as const satisfies readonly (keyof NotificationKindPolicy)[];

  let { open, workspace, hosts, notifications, onclose, onopensession }: Props = $props();
  let filter: "unread" | "all" = $state("unread");
  let drawer = $state<HTMLDivElement>();
  let closeButton = $state<HTMLButtonElement>();
  let previouslyFocused: HTMLElement | undefined;
  let focusOwned = false;
  const pending = new SvelteSet<string>();
  let policyHost = $state("");
  let policy: NotificationPolicy | undefined = $state();
  let policyLoadedHost: string | undefined = $state();
  let policyProviders: string[] = $state([]);
  let policyLoading = $state(false);
  let policyResourceHost = "";
  const policyRequests = new LatestRequest();

  const connectedHosts = $derived(
    Object.values($hosts)
      .filter((entry) => entry.connection.kind === "connected")
      .sort((left, right) => left.host.localeCompare(right.host)),
  );

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
      policyRequests.invalidate();
      policyLoading = false;
      restoreFocus();
    }
  });

  $effect((): void => {
    if (!open) return;
    const firstHost = connectedHosts[0]?.host ?? "";
    if (
      !connectedHosts.some((entry) => entry.host === policyHost)
      && policyHost !== firstHost
    ) {
      policyHost = firstHost;
      return;
    }
    if (policyResourceHost !== policyHost) {
      policyResourceHost = policyHost;
      policyRequests.invalidate();
      policy = undefined;
      policyLoadedHost = undefined;
      policyProviders = [];
      policyLoading = false;
    }
  });

  onDestroy((): void => {
    policyRequests.invalidate();
    restoreFocus();
  });

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

  async function loadPolicy(): Promise<void> {
    if (policyHost.length === 0) return;
    const requestHost = policyHost;
    const token = policyRequests.begin(requestHost);
    policyLoading = true;
    try {
      const [result, capabilities] = await Promise.all([
        workspace.actions.notificationPolicyGet(requestHost),
        workspace.actions.hostInspect(requestHost),
      ]);
      if (!open || !policyRequests.isCurrent(token, policyHost)) return;
      policy = structuredClone(result.policy);
      policyLoadedHost = requestHost;
      policyProviders = Array.from(new Set([
        ...capabilities.runtimes.map((runtime) => runtime.agent),
        ...Object.keys(result.policy.providers ?? {}),
      ])).sort();
    } catch (error: unknown) {
      if (open && policyRequests.isCurrent(token, policyHost)) {
        addErrorToast(error);
      }
    } finally {
      if (policyRequests.isCurrent(token, policyHost)) {
        policyLoading = false;
      }
    }
  }

  function providerPolicy(provider: string): NotificationKindPolicy | undefined {
    return policy?.providers?.[provider];
  }

  function updateProviderPolicy(
    provider: string,
    kind: keyof NotificationKindPolicy,
    enabled: boolean,
  ): void {
    if (policy === undefined) return;
    // A same-host reload/save may still be in flight. The local edit is newer
    // than that response, so invalidate its token before updating the policy.
    policyRequests.invalidate();
    policyLoading = false;
    const current = providerPolicy(provider) ?? policy.enabled;
    policy = {
      ...policy,
      providers: {
        ...(policy.providers ?? {}),
        [provider]: { ...current, [kind]: enabled },
      },
    };
  }

  async function savePolicy(): Promise<void> {
    if (policy === undefined || policyHost.length === 0 || policyLoadedHost !== policyHost) return;
    const requestHost = policyHost;
    const requestPolicy = structuredClone(policy);
    const token = policyRequests.begin(requestHost);
    policyLoading = true;
    try {
      const result = await workspace.actions.notificationPolicySet(requestHost, { policy: requestPolicy });
      if (!open || !policyRequests.isCurrent(token, policyHost)) return;
      policy = structuredClone(result.policy);
      policyLoadedHost = requestHost;
    } catch (error: unknown) {
      if (open && policyRequests.isCurrent(token, policyHost)) {
        addErrorToast(error);
      }
    } finally {
      if (policyRequests.isCurrent(token, policyHost)) {
        policyLoading = false;
      }
    }
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

    <details class="policy-editor">
      <summary>Notification policy</summary>
      <div class="policy-toolbar">
        <label>
          Host
          <select bind:value={policyHost}>
            {#each connectedHosts as host (host.host)}
              <option value={host.host}>{host.host}</option>
            {/each}
          </select>
        </label>
        <button type="button" disabled={policyLoading || policyHost.length === 0} onclick={() => void loadPolicy()}>
          {policy === undefined ? "Load policy" : "Reload"}
        </button>
      </div>
      {#if policy !== undefined}
        <p class="policy-help">Provider rows override the base policy. Unknown future provider keys are preserved.</p>
        <div class="policy-table-wrap">
          <table class="policy-table">
            <thead>
              <tr><th>Provider</th>{#each NOTIFICATION_KINDS as kind (kind)}<th>{kind.replaceAll("_", " ")}</th>{/each}</tr>
            </thead>
            <tbody>
              {#each policyProviders as provider (provider)}
                <tr>
                  <th>{provider}</th>
                  {#each NOTIFICATION_KINDS as kind (kind)}
                    <td>
                      <input
                        type="checkbox"
                        aria-label={`${provider} ${kind}`}
                        checked={providerPolicy(provider)?.[kind] ?? policy.enabled[kind]}
                        onchange={(event) => updateProviderPolicy(
                          provider,
                          kind,
                          event.currentTarget.checked,
                        )}
                      />
                    </td>
                  {/each}
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
        <button
          type="button"
          disabled={policyLoading || policyLoadedHost !== policyHost}
          onclick={() => void savePolicy()}
        >Save policy</button>
      {/if}
    </details>

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

  .policy-editor {
    margin-bottom: 1rem;
    padding: 0.75rem;
    border: 1px solid var(--border);
    border-radius: 0.55rem;
    background: var(--surface);
  }

  .policy-editor summary {
    cursor: pointer;
    font-weight: 600;
  }

  .policy-toolbar {
    display: flex;
    align-items: end;
    gap: 0.5rem;
    margin-top: 0.75rem;
  }

  .policy-toolbar label {
    display: grid;
    gap: 0.25rem;
  }

  .policy-help {
    color: var(--muted);
    font-size: 0.8rem;
  }

  .policy-table-wrap {
    overflow-x: auto;
    margin-bottom: 0.75rem;
  }

  .policy-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.75rem;
  }

  .policy-table th,
  .policy-table td {
    padding: 0.35rem;
    border-bottom: 1px solid var(--border);
    text-align: center;
  }

  .policy-table th:first-child {
    text-align: left;
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
