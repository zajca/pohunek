<script lang="ts">
  import type {
    HostedSession,
    HostsSnapshot,
    SessionsSnapshot,
  } from "@pohunek/client-core";
  import { SvelteMap } from "svelte/reactivity";
  import type { Readable } from "svelte/store";
  import { selectPrimaryHosts } from "../lib/host-presentation";

  interface Props {
    hosts: Readable<HostsSnapshot>;
    sessions: Readable<SessionsSnapshot>;
    selectedHost: string | undefined;
    selectedSessionId: string | undefined;
    collapsed?: boolean;
    onselect: (host: string, sessionId: string) => void;
    onnewsession: () => void;
    onclose?: (() => void) | undefined;
  }

  type SessionFilter = "all" | "blocked" | "working" | "idle" | "finished";

  interface SessionGroup {
    readonly key: string;
    readonly label: string;
    readonly entries: readonly HostedSession[];
  }

  let {
    hosts,
    sessions,
    selectedHost,
    selectedSessionId,
    collapsed = false,
    onselect,
    onnewsession,
    onclose,
  }: Props = $props();
  let query = $state("");
  let filter: SessionFilter = $state("all");

  const allEntries = $derived(Object.values($sessions));
  const blockedCount = $derived(allEntries.filter((entry) => entry.session.activity === "blocked").length);
  const visibleEntries = $derived(
    allEntries
      .filter((entry) => matchesFilter(entry, filter))
      .filter((entry) => matchesQuery(entry, query))
      .sort(compareSessions),
  );
  const hasVisibleSelection = $derived(visibleEntries.some((entry) => isSelected(entry)));
  const groups = $derived(groupSessions(visibleEntries));
  const primaryHosts = $derived(selectPrimaryHosts($hosts));

  function select(entry: HostedSession): void {
    onselect(entry.host, entry.session.id);
  }

  function setFilter(next: SessionFilter): void {
    filter = next;
  }

  function matchesFilter(entry: HostedSession, selectedFilter: SessionFilter): boolean {
    if (selectedFilter === "all") {
      return true;
    }
    if (selectedFilter === "finished") {
      return entry.session.state === "done"
        || entry.session.state === "failed"
        || entry.session.state === "stopped";
    }
    return entry.session.activity === selectedFilter;
  }

  function matchesQuery(entry: HostedSession, rawQuery: string): boolean {
    const normalizedQuery = rawQuery.trim().toLocaleLowerCase();
    if (normalizedQuery.length === 0) {
      return true;
    }
    const session = entry.session;
    return [
      session.name,
      session.id,
      session.project_label,
      session.project_id,
      session.repo,
      session.branch,
      session.agent,
      session.active_agent,
      session.cwd,
      entry.host,
    ].some((value) => value?.toLocaleLowerCase().includes(normalizedQuery) === true);
  }

  function compareSessions(left: HostedSession, right: HostedSession): number {
    const activityDifference = activityPriority(left) - activityPriority(right);
    if (activityDifference !== 0) {
      return activityDifference;
    }
    return right.session.updated_at.localeCompare(left.session.updated_at);
  }

  function activityPriority(entry: HostedSession): number {
    switch (entry.session.activity) {
      case "blocked":
        return 0;
      case "working":
        return 1;
      case "idle":
        return 2;
      case undefined:
        return 3;
    }
    return 3;
  }

  function groupSessions(entries: readonly HostedSession[]): readonly SessionGroup[] {
    const attention = entries.filter((entry) => entry.session.activity === "blocked");
    const projectGroups = new SvelteMap<string, { label: string; entries: HostedSession[] }>();
    for (const entry of entries) {
      if (entry.session.activity === "blocked") {
        continue;
      }
      const label = projectLabel(entry);
      const key = entry.session.project_id ?? entry.session.repo ?? label;
      const group = projectGroups.get(key) ?? { label, entries: [] };
      group.entries.push(entry);
      projectGroups.set(key, group);
    }

    const grouped: SessionGroup[] = attention.length === 0
      ? []
      : [{ key: "attention", label: `Attention · ${attention.length}`, entries: attention }];
    grouped.push(
      ...Array.from(projectGroups, ([key, group]): SessionGroup => ({
        key,
        label: group.label,
        entries: group.entries,
      })).sort((left, right) => left.label.localeCompare(right.label)),
    );
    return grouped;
  }

  function projectLabel(entry: HostedSession): string {
    if (entry.session.project_label !== undefined) {
      return entry.session.project_label;
    }
    if (entry.session.repo !== undefined) {
      const segments = entry.session.repo.split(/[\\/]/).filter((segment) => segment.length > 0);
      return segments.at(-1) ?? entry.session.repo;
    }
    return "Other sessions";
  }

  function sessionLabel(entry: HostedSession): string {
    return entry.session.name ?? entry.session.branch ?? entry.session.id;
  }

  function sessionInitial(entry: HostedSession): string {
    return sessionLabel(entry).trim().charAt(0).toLocaleUpperCase() || "S";
  }

  function activityLabel(entry: HostedSession): string {
    return entry.session.activity ?? "Activity unknown";
  }

  function isSelected(entry: HostedSession): boolean {
    return entry.host === selectedHost && entry.session.id === selectedSessionId;
  }

  function railTabIndex(entry: HostedSession): 0 | -1 {
    return isSelected(entry) || (!hasVisibleSelection && visibleEntries[0] === entry) ? 0 : -1;
  }
</script>

<aside class:rail-collapsed={collapsed} class="session-rail" aria-label="Sessions">
  <div class="rail-heading">
    {#if !collapsed}
      <div>
        <span class="rail-eyebrow">Control plane</span>
        <h1>Sessions</h1>
      </div>
    {/if}
    <div class="rail-heading-actions">
      <button class="rail-new-button" type="button" onclick={onnewsession} aria-label="New session" title="New session">
        <span aria-hidden="true">+</span>
        {#if !collapsed}<span>New</span>{/if}
      </button>
      {#if onclose !== undefined}
        <!-- svelte-ignore a11y_autofocus (modal navigation must receive focus when mounted) -->
        <button
          class="rail-close-button"
          type="button"
          autofocus
          onclick={onclose}
          aria-label="Close session navigation"
        >
          <span aria-hidden="true">×</span>
        </button>
      {/if}
    </div>
  </div>

  {#if !collapsed}
    <div class="rail-tools">
      <label class="visually-hidden" for="session-search">Search sessions</label>
      <input
        id="session-search"
        data-session-search
        type="search"
        placeholder="Search sessions…"
        bind:value={query}
      />
      <div class="session-filters" aria-label="Filter sessions">
        {#each ["all", "blocked", "working", "idle", "finished"] as option (option)}
          <button
            type="button"
            class:filter-selected={filter === option}
            aria-pressed={filter === option}
            onclick={() => setFilter(option as SessionFilter)}
          >
            {option === "all" ? "All" : option.charAt(0).toLocaleUpperCase() + option.slice(1)}
            {#if option === "blocked" && blockedCount > 0}<span>{blockedCount}</span>{/if}
          </button>
        {/each}
      </div>
    </div>
  {/if}

  <div class="session-list" data-testid="session-list">
    {#if allEntries.length === 0}
      {#if !collapsed}<p class="rail-empty">Waiting for sessions…</p>{/if}
    {:else if groups.length === 0}
      {#if !collapsed}<p class="rail-empty">No sessions match this view.</p>{/if}
    {:else}
      {#each groups as group (group.key)}
        <section class="session-group" aria-labelledby={`session-group-${group.key}`}>
          {#if !collapsed}
            <h2 id={`session-group-${group.key}`} class:attention-heading={group.key === "attention"}>
              {group.label}
            </h2>
          {/if}
          <ul>
            {#each group.entries as entry (`${entry.host}:${entry.session.id}`)}
              <li>
                <button
                  type="button"
                  class="session-item"
                  class:session-selected={isSelected(entry)}
                  class:activity-blocked={entry.session.activity === "blocked"}
                  class:activity-working={entry.session.activity === "working"}
                  class:activity-idle={entry.session.activity === "idle"}
                  data-testid="session-row"
                  data-host={entry.host}
                  data-session-id={entry.session.id}
                  aria-current={isSelected(entry) ? "page" : undefined}
                  tabindex={railTabIndex(entry)}
                  aria-label={`${sessionLabel(entry)}, ${activityLabel(entry)}, ${entry.host}`}
                  title={collapsed ? `${sessionLabel(entry)} · ${entry.host}` : undefined}
                  onclick={() => select(entry)}
                >
                  {#if collapsed}
                    <span class="session-initial" aria-hidden="true">{sessionInitial(entry)}</span>
                    <span
                      class="activity-dot"
                      class:activity-unknown={entry.session.activity === undefined}
                      data-agent-state={entry.session.activity ?? "unknown"}
                      data-state-source={entry.session.activity === undefined ? undefined : entry.session.state_source}
                    ></span>
                  {:else}
                    <span class="session-item-main">
                      <strong>{sessionLabel(entry)}</strong>
                      <span>{entry.session.branch ?? entry.session.agent} · {entry.host}</span>
                    </span>
                    <span class="session-item-state">
                      <span
                        class="activity-label"
                        class:activity-unknown={entry.session.activity === undefined}
                        data-agent-state={entry.session.activity ?? "unknown"}
                        data-state-source={entry.session.activity === undefined ? undefined : entry.session.state_source}
                      >
                        <span class="activity-dot" aria-hidden="true"></span>
                        {activityLabel(entry)}
                      </span>
                      <span class="lifecycle-label">{entry.session.state}</span>
                    </span>
                  {/if}
                </button>
              </li>
            {/each}
          </ul>
        </section>
      {/each}
    {/if}
  </div>

  {#if primaryHosts.reachableDaemons.length > 0 || primaryHosts.versionMismatches.length > 0}
    <div class="host-strip" aria-label="Host connectivity">
      {#if primaryHosts.reachableDaemons.length === 1}
        {@const host = primaryHosts.reachableDaemons[0]!}
        <span
          class="host-summary"
          data-testid="host-card"
          data-host={host.host}
          title={`${host.host}: ${host.connection.kind.replace("_", " ")}`}
        >
          <span class={`host-dot host-${host.connection.kind}`} data-connection={host.connection.kind}></span>
          {#if !collapsed}<span>{host.connection.kind === "connected" ? "Online" : "Unavailable"} · {host.host}</span>{/if}
        </span>
      {:else}
        {#each primaryHosts.reachableDaemons as host (host.host)}
          <span
            class="host-summary"
            data-testid="host-card"
            data-host={host.host}
            title={`${host.host}: ${host.connection.kind.replace("_", " ")}`}
          >
            <span class={`host-dot host-${host.connection.kind}`} data-connection={host.connection.kind}></span>
            {#if !collapsed}<span>{host.host}</span>{/if}
          </span>
        {/each}
      {/if}

      {#each primaryHosts.versionMismatches as host (host.host)}
        <span
          class="host-version-warning"
          data-testid="host-card"
          data-host={host.host}
          title={`${host.host}: daemon version mismatch`}
        >
          <span class="host-dot host-version_mismatch" data-connection="version_mismatch"></span>
          {#if !collapsed}<span>Update {host.host}</span>{/if}
        </span>
      {/each}
    </div>
  {/if}
</aside>
