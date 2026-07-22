<script lang="ts">
  import type { NotificationsSnapshot, Workspace } from "@pohunek/client-core";
  import type { Readable } from "svelte/store";
  import InboxDrawer from "../components/InboxDrawer.svelte";
  import type { HistoryRouter } from "../lib";

  interface Props {
    router: HistoryRouter;
    workspace: Workspace;
    notifications: Readable<NotificationsSnapshot>;
  }

  let { router, workspace, notifications }: Props = $props();
</script>

<InboxDrawer
  open={true}
  {workspace}
  {notifications}
  onclose={() => router.navigate({ kind: "workspace" })}
  onopensession={(host, sessionId) => router.navigate({ kind: "terminal", host, sessionId })}
/>
