<script lang="ts">
  import type {
    SessionAttachment,
    Workspace,
  } from "@pohunek/client-core";
  import { FitAddon } from "@xterm/addon-fit";
  import { WebglAddon } from "@xterm/addon-webgl";
  import { Terminal, type IDisposable } from "@xterm/xterm";
  import { onMount } from "svelte";
  import {
    addErrorToast,
    type HistoryRouter,
  } from "../lib";

  interface Props {
    router: HistoryRouter;
    workspace: Workspace;
    host: string;
    sessionId: string;
  }

  const RESIZE_DEBOUNCE_MS = 140;
  const RESIZE_MAX_WAIT_MS = 500;
  const encoder = new TextEncoder();

  let { router, workspace, host, sessionId }: Props = $props();
  let terminalElement: HTMLDivElement;
  let status = $state("Attaching…");
  let attachment: SessionAttachment | undefined;
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  let writer: WritableStreamDefaultWriter<Uint8Array> | undefined;
  let resizeTimer: ReturnType<typeof setTimeout> | undefined;
  let resizeBurstStartedAt: number | undefined;
  let pendingResize: { readonly cols: number; readonly rows: number } | undefined;
  let teardownTask: Promise<void> | undefined;
  let closing = false;

  onMount((): (() => void) => {
    const terminal = new Terminal({
      cursorBlink: true,
      screenReaderMode: true,
      fontFamily: '"Cascadia Mono", "SFMono-Regular", Consolas, monospace',
      fontSize: 14,
      scrollback: 10_000,
      theme: {
        background: "#05070b",
        foreground: "#e9edf5",
        cursor: "#9ac1ff",
        selectionBackground: "#38577a",
      },
    });
    const fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);
    terminal.open(terminalElement);

    let webglAddon: WebglAddon | undefined;
    let webglContextLoss: IDisposable | undefined;
    try {
      webglAddon = new WebglAddon();
      webglContextLoss = webglAddon.onContextLoss((): void => {
        webglAddon?.dispose();
        webglAddon = undefined;
      });
      terminal.loadAddon(webglAddon);
    } catch {
      webglAddon?.dispose();
      webglAddon = undefined;
    }

    const resizeObserver = new ResizeObserver((): void => {
      fitAddon.fit();
    });
    resizeObserver.observe(terminalElement);
    fitAddon.fit();

    const inputSubscription = terminal.onData((data): void => {
      const activeWriter = writer;
      if (activeWriter !== undefined) {
        void activeWriter.write(encoder.encode(data)).catch((error: unknown): void => {
          if (!closing) {
            addErrorToast(error);
          }
        });
      }
    });
    const resizeSubscription = terminal.onResize(({ cols, rows }): void => {
      queueResize(cols, rows);
    });
    const beforeUnload = (): void => {
      void attachment?.detach();
    };
    window.addEventListener("beforeunload", beforeUnload);

    void connect(terminal);

    return (): void => {
      closing = true;
      window.removeEventListener("beforeunload", beforeUnload);
      resizeObserver.disconnect();
      inputSubscription.dispose();
      resizeSubscription.dispose();
      webglContextLoss?.dispose();
      webglAddon?.dispose();
      if (resizeTimer !== undefined) {
        clearTimeout(resizeTimer);
        resizeTimer = undefined;
      }
      pendingResize = undefined;
      resizeBurstStartedAt = undefined;
      void teardown().finally((): void => terminal.dispose());
    };
  });

  async function connect(terminal: Terminal): Promise<void> {
    try {
      const activeAttachment = await workspace.attach(host, sessionId);
      if (closing) {
        await activeAttachment.detach();
        return;
      }
      attachment = activeAttachment;
      reader = activeAttachment.stream.readable.getReader();
      writer = activeAttachment.stream.writable.getWriter();
      status = "Attached";
      queueResize(terminal.cols, terminal.rows);
      await readTerminalOutput(terminal, reader);
    } catch (error: unknown) {
      if (!closing) {
        status = "Attach failed";
        addErrorToast(error);
      }
    }
  }

  async function readTerminalOutput(
    terminal: Terminal,
    activeReader: ReadableStreamDefaultReader<Uint8Array>,
  ): Promise<void> {
    try {
      while (!closing) {
        const next = await activeReader.read();
        if (next.done) {
          if (!closing) {
            status = "Detached";
          }
          return;
        }
        terminal.write(next.value);
      }
    } finally {
      if (reader === activeReader) {
        reader = undefined;
      }
      activeReader.releaseLock();
    }
  }

  function queueResize(cols: number, rows: number): void {
    if (attachment === undefined || closing) {
      return;
    }
    pendingResize = { cols, rows };
    const now = performance.now();
    resizeBurstStartedAt ??= now;
    const maxWaitRemaining = Math.max(0, RESIZE_MAX_WAIT_MS - (now - resizeBurstStartedAt));
    const delay = Math.min(RESIZE_DEBOUNCE_MS, maxWaitRemaining);
    if (resizeTimer !== undefined) {
      clearTimeout(resizeTimer);
    }
    resizeTimer = setTimeout(flushResize, delay);
  }

  function flushResize(): void {
    resizeTimer = undefined;
    resizeBurstStartedAt = undefined;
    const resize = pendingResize;
    pendingResize = undefined;
    if (resize === undefined || closing) {
      return;
    }
    void workspace.actions.sessionResize(host, {
      session_id: sessionId,
      cols: resize.cols,
      rows: resize.rows,
    }).catch((error: unknown): void => {
      addErrorToast(error);
    });
  }

  function teardown(): Promise<void> {
    teardownTask ??= closeAttachment();
    return teardownTask;
  }

  async function closeAttachment(): Promise<void> {
    closing = true;
    if (resizeTimer !== undefined) {
      clearTimeout(resizeTimer);
      resizeTimer = undefined;
    }
    pendingResize = undefined;
    resizeBurstStartedAt = undefined;
    writer?.releaseLock();
    writer = undefined;
    const activeAttachment = attachment;
    attachment = undefined;
    if (activeAttachment !== undefined) {
      try {
        await activeAttachment.detach();
      } catch (error: unknown) {
        addErrorToast(error);
      }
    }
    status = "Detached";
  }

  async function closeView(): Promise<void> {
    await teardown();
    router.navigate({ kind: "session", host, sessionId });
  }
</script>

<main class="terminal-page">
  <div class="terminal-toolbar">
    <div>
      <strong>{host} / {sessionId}</strong>
      <span class="muted" data-testid="terminal-status"> · {status}</span>
    </div>
    <button type="button" onclick={() => void closeView()}>Close terminal</button>
  </div>
  <div
    class="terminal-host"
    bind:this={terminalElement}
    data-testid="terminal"
    aria-label={`Terminal for ${sessionId} on ${host}`}
  ></div>
</main>
