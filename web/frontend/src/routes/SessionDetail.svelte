<script lang="ts">
  import type { SessionsSnapshot, Workspace } from "@pohunek/client-core";
  import type { Readable } from "svelte/store";
  import SessionInspector from "../components/SessionInspector.svelte";
  import type { HistoryRouter } from "../lib";

  interface Props {
    router: HistoryRouter;
    workspace: Workspace;
    sessions: Readable<SessionsSnapshot>;
    host: string;
    sessionId: string;
  }

  let { router, workspace, sessions, host, sessionId }: Props = $props();
</script>

<SessionInspector
  open={true}
  {workspace}
  {sessions}
  {host}
  {sessionId}
  onclose={() => router.navigate({ kind: "workspace" })}
  onopenterminal={(terminalHost, terminalSessionId) => router.navigate({
    kind: "terminal",
    host: terminalHost,
    sessionId: terminalSessionId,
  })}
/>
