<script lang="ts">
  interface Props {
    activity?: "working" | "blocked" | "idle";
    source: string;
  }

  let { activity = "idle", source }: Props = $props();
</script>

<span
  class={`agent-badge badge-${activity}`}
  data-agent-state={activity}
  data-state-source={source}
  title={`Reported by ${source}`}
>
  {activity} · {source}
</span>
