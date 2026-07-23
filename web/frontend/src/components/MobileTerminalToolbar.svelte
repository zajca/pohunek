<script lang="ts">
  import {
    clearTerminalModifiers,
    NO_TERMINAL_MODIFIERS,
    toggleTerminalModifier,
    type TerminalModifier,
    type TerminalModifiers,
    type TerminalToolbarKey,
  } from "../lib/terminal-keys";

  interface Props {
    attached: boolean;
    onfocus: () => void;
    onsend: (key: TerminalToolbarKey, modifiers: TerminalModifiers) => void;
  }

  let { attached, onfocus, onsend }: Props = $props();
  let modifiers: TerminalModifiers = $state(NO_TERMINAL_MODIFIERS);

  $effect((): void => {
    if (!attached) {
      modifiers = NO_TERMINAL_MODIFIERS;
    }
  });

  function toggle(modifier: TerminalModifier): void {
    if (attached) {
      modifiers = toggleTerminalModifier(modifiers, modifier);
    }
  }

  function send(key: TerminalToolbarKey, forceCtrl = false): void {
    if (!attached) {
      return;
    }
    onsend(key, forceCtrl ? { ...modifiers, ctrl: true } : modifiers);
    modifiers = clearTerminalModifiers();
  }
</script>

<div class="mobile-terminal-controls" role="toolbar" aria-label="Mobile terminal controls">
  <button
    class="keyboard-focus"
    type="button"
    disabled={!attached}
    aria-label="Focus terminal and open keyboard"
    onclick={onfocus}
  >Keyboard</button>
  <button type="button" disabled={!attached} aria-label="Send Escape" onclick={() => send("escape")}>Esc</button>
  <button type="button" disabled={!attached} aria-label="Send Tab" onclick={() => send("tab")}>Tab</button>
  <button
    class:modifier-latched={modifiers.ctrl}
    type="button"
    disabled={!attached}
    aria-label="Toggle Control modifier"
    aria-pressed={modifiers.ctrl}
    onclick={() => toggle("ctrl")}
  >Ctrl</button>
  <button
    class:modifier-latched={modifiers.alt}
    type="button"
    disabled={!attached}
    aria-label="Toggle Alt modifier"
    aria-pressed={modifiers.alt}
    onclick={() => toggle("alt")}
  >Alt</button>
  <button type="button" disabled={!attached} aria-label="Send Control C" onclick={() => send("c", true)}>Ctrl+C</button>
  <span class="arrow-controls" role="group" aria-label="Terminal arrow keys">
    <button type="button" disabled={!attached} aria-label="Send left arrow" onclick={() => send("arrow-left")}>←</button>
    <button type="button" disabled={!attached} aria-label="Send up arrow" onclick={() => send("arrow-up")}>↑</button>
    <button type="button" disabled={!attached} aria-label="Send down arrow" onclick={() => send("arrow-down")}>↓</button>
    <button type="button" disabled={!attached} aria-label="Send right arrow" onclick={() => send("arrow-right")}>→</button>
  </span>
</div>

<style>
  .mobile-terminal-controls {
    display: none;
  }

  @media (pointer: coarse), (max-width: 760px) {
    .mobile-terminal-controls {
      display: flex;
      min-width: 0;
      flex: 0 0 auto;
      gap: 0.35rem;
      overflow-x: auto;
      padding: 0.4rem 0;
      scrollbar-width: thin;
      touch-action: manipulation;
    }

    button {
      min-width: 44px;
      min-height: 44px;
      flex: 0 0 auto;
      padding: 0.45rem 0.65rem;
      border-radius: 0.45rem;
      font-size: 0.78rem;
      user-select: none;
      -webkit-tap-highlight-color: transparent;
    }

    .keyboard-focus {
      min-width: 5.4rem;
      border-color: #42699c;
      background: #1d3557;
    }

    .modifier-latched {
      border-color: var(--accent);
      color: #fff;
      background: #285486;
      box-shadow: inset 0 0 0 1px var(--accent);
    }

    .arrow-controls {
      display: flex;
      gap: 0.25rem;
    }
  }
</style>
