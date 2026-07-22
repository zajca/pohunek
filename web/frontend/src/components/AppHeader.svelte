<script lang="ts">
  import type { HistoryRouter } from "../lib";

  interface Props {
    router: HistoryRouter;
    unreadCount: number;
  }

  let { router, unreadCount }: Props = $props();

  function navigateHome(event: MouseEvent): void {
    event.preventDefault();
    router.navigate({ kind: "workspace" });
  }

  function navigateInbox(event: MouseEvent): void {
    event.preventDefault();
    router.navigate({ kind: "inbox" });
  }
</script>

<header class="app-header">
  <a class="brand" href={router.href({ kind: "workspace" })} onclick={navigateHome}>
    Pohunek Control Center
  </a>
  <nav class="header-nav" aria-label="Primary navigation">
    <a class="nav-link" href={router.href({ kind: "workspace" })} onclick={navigateHome}>
      Workspace
    </a>
    <a class="nav-link" href={router.href({ kind: "inbox" })} onclick={navigateInbox}>
      Inbox
      {#if unreadCount > 0}
        <span class="unread-count" data-testid="unread-count">{unreadCount}</span>
      {/if}
    </a>
  </nav>
</header>
