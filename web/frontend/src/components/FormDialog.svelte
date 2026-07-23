<script lang="ts">
  import type { Snippet } from "svelte";

  interface Props {
    open: boolean;
    title: string;
    submitLabel?: string;
    submitting?: boolean;
    onclose: () => void;
    onsubmit: () => void;
    children?: Snippet;
  }

  let {
    open,
    title,
    submitLabel = "Save",
    submitting = false,
    onclose,
    onsubmit,
    children,
  }: Props = $props();
  let dialog: HTMLDialogElement;

  $effect((): void => {
    if (open && !dialog.open) {
      dialog.showModal();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  });

  function submit(event: SubmitEvent): void {
    event.preventDefault();
    onsubmit();
  }
</script>

<dialog
  bind:this={dialog}
  aria-label={title}
  oncancel={(event) => {
    event.preventDefault();
    if (!submitting) {
      onclose();
    }
  }}
>
  <form onsubmit={submit}>
    <h2>{title}</h2>
    {@render children?.()}
    <div class="actions-row">
      <button type="button" onclick={onclose} disabled={submitting}>Cancel</button>
      <button class="button-primary" type="submit" disabled={submitting}>
        {submitting ? "Saving…" : submitLabel}
      </button>
    </div>
  </form>
</dialog>

<style>
  form {
    display: grid;
    gap: 1rem;
  }

  h2 {
    margin: 0;
  }
</style>
