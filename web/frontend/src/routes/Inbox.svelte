<script lang="ts">
  import type { HostsSnapshot, NotificationsSnapshot, Workspace } from "@pohunek/client-core";
  import type { Readable } from "svelte/store";
  import InboxDrawer from "../components/InboxDrawer.svelte";
  import type { HistoryRouter } from "../lib";

  interface Props {
    router: HistoryRouter;
    workspace: Workspace;
    hosts: Readable<HostsSnapshot>;
    notifications: Readable<NotificationsSnapshot>;
  }

  let { router, workspace, hosts, notifications }: Props = $props();
</script>

<InboxDrawer
  open={true}
  {workspace}
  {hosts}
  {notifications}
  onclose={() => router.navigate({ kind: "workspace" })}
  onopensession={(host, sessionId) => router.navigate({ kind: "terminal", host, sessionId })}
/>
