<script lang="ts">
  import type { Snippet } from "svelte";
  interface Props {
    open: boolean;
    title: string;
    message: string;
    confirmLabel?: string;
    onconfirm: () => void;
    oncancel: () => void;
    children?: Snippet;
  }

  let {
    open,
    title,
    message,
    confirmLabel = "Confirm",
    onconfirm,
    oncancel,
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
</script>

<dialog
  bind:this={dialog}
  aria-label={title}
  oncancel={(event) => { event.preventDefault(); oncancel(); }}
>
  <h2>{title}</h2>
  <p>{message}</p>
  {@render children?.()}
  <div class="actions-row">
    <button type="button" onclick={oncancel}>Cancel</button>
    <button class="button-danger" type="button" onclick={onconfirm}>{confirmLabel}</button>
  </div>
</dialog>
