<script lang="ts">
  import type { HostsSnapshot, Workspace } from "@pohunek/client-core";
  import type { Readable } from "svelte/store";
  import NewSessionDialog from "../components/NewSessionDialog.svelte";
  import type { HistoryRouter } from "../lib";

  interface Props {
    router: HistoryRouter;
    workspace: Workspace;
    hosts: Readable<HostsSnapshot>;
    selectedRouteHost?: string | undefined;
  }

  let { router, workspace, hosts, selectedRouteHost }: Props = $props();
</script>

<NewSessionDialog
  open={true}
  {workspace}
  {hosts}
  selectedHost={selectedRouteHost}
  onclose={() => router.navigate({ kind: "workspace" })}
  oncreated={(host, sessionId) => router.navigate({ kind: "terminal", host, sessionId })}
/>
