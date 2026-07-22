<script lang="ts">
  import type { HostConnectionState } from "@pohunek/client-core";

  interface Props {
    state: HostConnectionState;
  }

  let { state }: Props = $props();

  const label = $derived.by((): string => {
    switch (state.kind) {
      case "connecting":
        return "Connecting";
      case "connected":
        return "Connected";
      case "error":
        return "Connection error";
      case "version_mismatch":
        return `Version mismatch (${state.theirs})`;
    }
  });

  const detail = $derived(state.kind === "error" ? state.reason : undefined);
</script>

<span
  class={`connection-marker marker-${state.kind}`}
  data-connection={state.kind}
  title={detail}
>
  {label}
</span>
