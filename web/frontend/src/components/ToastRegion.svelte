<script lang="ts">
  import { dismissToast, toasts } from "../lib";
</script>

<div class="toast-region" aria-live="polite" aria-label="Notifications">
  {#each $toasts as toast (toast.id)}
    <div class="toast" role={toast.kind === "error" ? "alert" : "status"} data-toast-kind={toast.kind}>
      <div>
        <strong>{toast.kind === "error" ? "Request failed" : toast.kind === "success" ? "Done" : "Notice"}</strong>
        <div><code>{toast.message}</code></div>
      </div>
      <button type="button" aria-label="Dismiss notification" onclick={() => dismissToast(toast.id)}>×</button>
    </div>
  {/each}
</div>
