<script lang="ts">
  import { onMount } from "svelte";

  interface TerminalSize {
    readonly cols: number;
    readonly rows: number;
  }

  interface Props {
    onchange: (size: TerminalSize) => void;
  }

  const MIN_TERMINAL_COLS = 20;
  const MIN_TERMINAL_ROWS = 5;
  const PROBE_GLYPH_COUNT = 10;
  const PROBE_TEXT = "M".repeat(PROBE_GLYPH_COUNT);

  let { onchange }: Props = $props();
  let container: HTMLDivElement;
  let glyph: HTMLSpanElement;
  let measured: TerminalSize | undefined = $state();

  onMount((): (() => void) => {
    const measure = (): void => {
      const containerRect = container.getBoundingClientRect();
      const glyphRect = glyph.getBoundingClientRect();
      const cellWidth = glyphRect.width / PROBE_GLYPH_COUNT;
      const cellHeight = glyphRect.height;
      if (containerRect.width <= 0 || containerRect.height <= 0 || cellWidth <= 0 || cellHeight <= 0) {
        return;
      }
      const next = {
        cols: Math.max(MIN_TERMINAL_COLS, Math.floor(containerRect.width / cellWidth)),
        rows: Math.max(MIN_TERMINAL_ROWS, Math.floor(containerRect.height / cellHeight)),
      };
      if (next.cols !== measured?.cols || next.rows !== measured?.rows) {
        measured = next;
        onchange(next);
      }
    };

    const observer = new ResizeObserver(measure);
    observer.observe(container);
    document.fonts.ready.then(measure).catch((): void => measure());
    measure();
    return (): void => observer.disconnect();
  });
</script>

<div class="terminal-probe" bind:this={container} data-testid="terminal-size-probe">
  <span class="terminal-probe-glyph" bind:this={glyph}>{PROBE_TEXT}</span>
  <span class="terminal-probe-label">
    {#if measured === undefined}
      Measuring terminal…
    {:else}
      Terminal size: {measured.cols} × {measured.rows}
    {/if}
  </span>
</div>
