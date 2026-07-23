<script lang="ts">
  import type { SessionsSnapshot } from "@pohunek/client-core";
  import { tick } from "svelte";
  import type { Readable } from "svelte/store";

  interface Props {
    open: boolean;
    sessions: Readable<SessionsSnapshot>;
    onclose: () => void;
    onnewsession: () => void;
    oninbox: () => void;
    onblocked: () => void;
    onopensession: (host: string, sessionId: string) => void;
  }

  interface PaletteEntry {
    readonly id: string;
    readonly label: string;
    readonly description: string;
    readonly keywords: string;
    readonly run: () => void;
  }

  let { open, sessions, onclose, onnewsession, oninbox, onblocked, onopensession }: Props = $props();
  let dialog: HTMLDialogElement;
  let input: HTMLInputElement;
  let query = $state("");
  let selectedIndex = $state(0);

  const entries = $derived.by((): readonly PaletteEntry[] => {
    const actions: readonly PaletteEntry[] = [
      {
        id: "action:new-session",
        label: "New session",
        description: "Start an agent on a connected host",
        keywords: "create launch agent",
        run: onnewsession,
      },
      {
        id: "action:inbox",
        label: "Open inbox",
        description: "Review notifications from every host",
        keywords: "notifications unread",
        run: oninbox,
      },
      {
        id: "action:blocked",
        label: "Next blocked session",
        description: "Jump to work that needs attention",
        keywords: "attention approval",
        run: onblocked,
      },
    ];
    const sessionEntries = Object.values($sessions)
      .sort((left, right) => right.session.updated_at.localeCompare(left.session.updated_at))
      .map((entry): PaletteEntry => ({
        id: `session:${entry.host}:${entry.session.id}`,
        label: entry.session.name ?? entry.session.id,
        description: [entry.session.project_label ?? entry.session.project_id, entry.session.branch, entry.host]
          .filter((value): value is string => value !== undefined)
          .join(" · "),
        keywords: [
          entry.host,
          entry.session.id,
          entry.session.agent,
          entry.session.project_label,
          entry.session.project_id,
          entry.session.branch,
          entry.session.repo,
          entry.session.activity,
        ].filter((value): value is string => value !== undefined).join(" "),
        run: () => onopensession(entry.host, entry.session.id),
      }));
    const normalizedQuery = query.trim().toLocaleLowerCase();
    const all = [...actions, ...sessionEntries];
    if (normalizedQuery.length === 0) {
      return all;
    }
    const tokens = normalizedQuery.split(/\s+/u);
    return all.filter((entry) => {
      const haystack = `${entry.label} ${entry.description} ${entry.keywords}`.toLocaleLowerCase();
      return tokens.every((token) => haystack.includes(token));
    });
  });

  $effect((): void => {
    if (open && !dialog.open) {
      query = "";
      selectedIndex = 0;
      dialog.showModal();
      void tick().then((): void => input.focus());
    } else if (!open && dialog.open) {
      dialog.close();
    }
  });

  $effect((): void => {
    if (selectedIndex >= entries.length) {
      selectedIndex = Math.max(0, entries.length - 1);
    }
  });

  function execute(index: number): void {
    const entry = entries[index];
    if (entry === undefined) {
      return;
    }
    onclose();
    entry.run();
  }

  function onKeydown(event: KeyboardEvent): void {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      onclose();
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      selectedIndex = entries.length === 0 ? 0 : (selectedIndex + 1) % entries.length;
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      selectedIndex = entries.length === 0 ? 0 : (selectedIndex - 1 + entries.length) % entries.length;
    } else if (event.key === "Enter") {
      event.preventDefault();
      execute(selectedIndex);
    } else if (event.key.toLowerCase() === "k" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      onclose();
    }
  }
</script>

<dialog
  bind:this={dialog}
  class="command-palette"
  aria-label="Command palette"
  oncancel={(event) => { event.preventDefault(); onclose(); }}
>
  <input
    bind:this={input}
    bind:value={query}
    type="search"
    placeholder="Search sessions or run a command…"
    aria-label="Search commands and sessions"
    aria-controls="command-results"
    aria-activedescendant={entries[selectedIndex] === undefined ? undefined : `command-${selectedIndex}`}
    onkeydown={onKeydown}
  />

  <div id="command-results" class="results" role="listbox" aria-label="Results">
    {#if entries.length === 0}
      <p>No matching sessions or commands.</p>
    {:else}
      {#each entries as entry, index (entry.id)}
        <button
          id={`command-${index}`}
          class:selected={selectedIndex === index}
          type="button"
          role="option"
          aria-selected={selectedIndex === index}
          onmouseenter={() => { selectedIndex = index; }}
          onclick={() => execute(index)}
        >
          <strong>{entry.label}</strong>
          <span>{entry.description}</span>
        </button>
      {/each}
    {/if}
  </div>
  <footer>
    <span>↑↓ navigate</span>
    <span>Enter open</span>
    <span>Esc close</span>
  </footer>
</dialog>

<style>
  .command-palette {
    width: min(42rem, calc(100vw - 2rem));
    max-height: min(36rem, calc(100vh - 4rem));
    padding: 0;
    overflow: hidden;
  }

  input {
    width: 100%;
    min-height: 3.5rem;
    padding: 0.9rem 1rem;
    border: 0;
    border-bottom: 1px solid var(--border);
    border-radius: 0;
    outline: none;
    color: #f5f7fb;
    background: #0c1119;
    font-size: 1rem;
  }

  .results {
    max-height: 27rem;
    overflow: auto;
    padding: 0.45rem;
  }

  .results button {
    display: grid;
    justify-content: stretch;
    width: 100%;
    min-height: auto;
    padding: 0.7rem 0.75rem;
    border-color: transparent;
    text-align: left;
    background: transparent;
  }

  .results button.selected {
    border-color: #365883;
    background: #1a2b41;
  }

  .results strong,
  .results span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .results span,
  .results p,
  footer {
    color: var(--muted);
    font-size: 0.78rem;
  }

  .results p {
    padding: 2rem;
    text-align: center;
  }

  footer {
    display: flex;
    gap: 1rem;
    padding: 0.55rem 1rem;
    border-top: 1px solid var(--border);
  }
</style>
