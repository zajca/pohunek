<script lang="ts">
  import type {
    HostsSnapshot,
    SessionsSnapshot,
  } from "@pohunek/client-core";
  import type { Readable } from "svelte/store";
  import AgentBadge from "../components/AgentBadge.svelte";
  import ConnectionMarker from "../components/ConnectionMarker.svelte";
  import { agentProfileLabel, type HistoryRouter } from "../lib";

  interface Props {
    router: HistoryRouter;
    hosts: Readable<HostsSnapshot>;
    sessions: Readable<SessionsSnapshot>;
  }

  let { router, hosts, sessions }: Props = $props();

  const hostEntries = $derived(Object.values($hosts).sort((left, right) => left.host.localeCompare(right.host)));
  const sessionEntries = $derived(
    Object.values($sessions).sort((left, right) => right.session.updated_at.localeCompare(left.session.updated_at)),
  );
  const connectedHosts = $derived(hostEntries.filter((host) => host.connection.kind === "connected"));

  function openNewSession(event: MouseEvent): void {
    event.preventDefault();
    router.navigate({ kind: "new-session" });
  }

  function openSession(event: MouseEvent, host: string, sessionId: string): void {
    event.preventDefault();
    router.navigate({ kind: "session", host, sessionId });
  }
</script>

<main class="page page-wide">
  <div class="page-heading">
    <div>
      <h1>Workspace</h1>
      <p class="muted">Live sessions across every discovered host.</p>
    </div>
    <a class="button button-primary" href={router.href({ kind: "new-session" })} onclick={openNewSession}>
      New session
    </a>
  </div>

  <section class="panel" aria-labelledby="hosts-heading">
    <div class="section-heading">
      <h2 id="hosts-heading">Hosts</h2>
      <span class="muted">{connectedHosts.length} of {hostEntries.length} connected</span>
    </div>

    {#if hostEntries.length === 0}
      <p class="empty-state">Waiting for host discovery…</p>
    {:else}
      <ul class="host-grid">
        {#each hostEntries as host (host.host)}
          <li class="host-card" data-testid="host-card" data-host={host.host}>
            <span class="host-name">{host.host}</span>
            <ConnectionMarker state={host.connection} />
            {#if host.daemon_version !== undefined}
              <div class="muted">Daemon {host.daemon_version}</div>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="panel" aria-labelledby="sessions-heading">
    <div class="section-heading">
      <h2 id="sessions-heading">Sessions</h2>
      <span class="muted">{sessionEntries.length} total</span>
    </div>

    {#if sessionEntries.length === 0}
      <p class="empty-state">No sessions are available yet.</p>
    {:else}
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Session</th>
              <th>Host</th>
              <th>Agent</th>
              <th>Agent state</th>
              <th>Lifecycle</th>
            </tr>
          </thead>
          <tbody>
            {#each sessionEntries as entry (`${entry.host}:${entry.session.id}`)}
              <tr data-testid="session-row" data-host={entry.host} data-session-id={entry.session.id}>
                <td>
                  <a
                    href={router.href({ kind: "session", host: entry.host, sessionId: entry.session.id })}
                    onclick={(event) => openSession(event, entry.host, entry.session.id)}
                  >
                    {entry.session.name ?? entry.session.id}
                  </a>
                </td>
                <td>{entry.host}</td>
                <td>{agentProfileLabel(entry.session.agent, entry.session.agent_base)}</td>
                <td>
                  <AgentBadge
                    activity={entry.session.activity ?? "idle"}
                    source={entry.session.state_source}
                  />
                </td>
                <td><span class="status-pill">{entry.session.state}</span></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </section>
</main>
