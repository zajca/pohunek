<script lang="ts">
  import type {
    SessionAttachment,
    Workspace,
  } from "@pohunek/client-core";
  import { FitAddon } from "@xterm/addon-fit";
  import { WebglAddon } from "@xterm/addon-webgl";
  import { Terminal, type IDisposable } from "@xterm/xterm";
  import { onMount } from "svelte";
  import { addErrorToast } from "../lib";

  interface Props {
    workspace: Workspace;
    host: string;
    sessionId: string;
  }

  const RESIZE_DEBOUNCE_MS = 140;
  const RESIZE_MAX_WAIT_MS = 500;
  const encoder = new TextEncoder();

  let { workspace, host, sessionId }: Props = $props();
  let terminalElement: HTMLDivElement;
  let terminal: Terminal | undefined;
  let status = $state("Attaching…");
  let failed = $state(false);
  let attachment: SessionAttachment | undefined;
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  let writer: WritableStreamDefaultWriter<Uint8Array> | undefined;
  let resizeTimer: ReturnType<typeof setTimeout> | undefined;
  let resizeBurstStartedAt: number | undefined;
  let pendingResize: { readonly cols: number; readonly rows: number } | undefined;
  let teardownTask: Promise<void> | undefined;
  let closing = false;
  let connecting = false;

  onMount((): (() => void) => {
    terminal = new Terminal({
      cursorBlink: true,
      screenReaderMode: true,
      fontFamily: '"Cascadia Mono", "SFMono-Regular", Consolas, monospace',
      fontSize: 14,
      lineHeight: 1.2,
      scrollback: 10_000,
      theme: {
        background: "#07090d",
        foreground: "#e8eaed",
        cursor: "#a8c7fa",
        selectionBackground: "#304b70",
      },
    });
    const activeTerminal = terminal;
    const fitAddon = new FitAddon();
    activeTerminal.loadAddon(fitAddon);
    activeTerminal.open(terminalElement);

    let webglAddon: WebglAddon | undefined;
    let webglContextLoss: IDisposable | undefined;
    try {
      webglAddon = new WebglAddon();
      webglContextLoss = webglAddon.onContextLoss((): void => {
        webglAddon?.dispose();
        webglAddon = undefined;
      });
      activeTerminal.loadAddon(webglAddon);
    } catch {
      webglAddon?.dispose();
      webglAddon = undefined;
    }

    const resizeObserver = new ResizeObserver((): void => {
      fitAddon.fit();
    });
    resizeObserver.observe(terminalElement);
    fitAddon.fit();

    const inputSubscription = activeTerminal.onData((data): void => {
      const activeWriter = writer;
      if (activeWriter !== undefined) {
        void activeWriter.write(encoder.encode(data)).catch((error: unknown): void => {
          if (!closing) {
            addErrorToast(error);
          }
        });
      }
    });
    const resizeSubscription = activeTerminal.onResize(({ cols, rows }): void => {
      queueResize(cols, rows);
    });
    const beforeUnload = (): void => {
      void attachment?.detach();
    };
    window.addEventListener("beforeunload", beforeUnload);

    void connect(activeTerminal);

    return (): void => {
      closing = true;
      window.removeEventListener("beforeunload", beforeUnload);
      resizeObserver.disconnect();
      inputSubscription.dispose();
      resizeSubscription.dispose();
      webglContextLoss?.dispose();
      webglAddon?.dispose();
      clearPendingResize();
      void teardown().finally((): void => activeTerminal.dispose());
      terminal = undefined;
    };
  });

  async function connect(activeTerminal: Terminal): Promise<void> {
    if (connecting || closing) {
      return;
    }
    connecting = true;
    status = "Attaching…";
    failed = false;
    let activeAttachment: SessionAttachment | undefined;
    try {
      activeAttachment = await workspace.attach(host, sessionId);
      if (closing) {
        await activeAttachment.detach();
        return;
      }
      attachment = activeAttachment;
      reader = activeAttachment.stream.readable.getReader();
      writer = activeAttachment.stream.writable.getWriter();
      status = "Attached";
      queueResize(activeTerminal.cols, activeTerminal.rows);
      await readTerminalOutput(activeTerminal, reader);
      await releaseEndedAttachment(activeAttachment);
    } catch (error: unknown) {
      await releaseEndedAttachment(activeAttachment);
      if (!closing) {
        status = "Attach failed";
        failed = true;
        addErrorToast(error);
      }
    } finally {
      connecting = false;
    }
  }

  async function retry(): Promise<void> {
    await closeAttachment(false);
    teardownTask = undefined;
    const activeTerminal = terminal;
    if (activeTerminal !== undefined && !closing) {
      void connect(activeTerminal);
    }
  }

  async function readTerminalOutput(
    activeTerminal: Terminal,
    activeReader: ReadableStreamDefaultReader<Uint8Array>,
  ): Promise<void> {
    try {
      while (!closing) {
        const next = await activeReader.read();
        if (next.done) {
          if (!closing) {
            status = "Detached";
            failed = true;
          }
          return;
        }
        activeTerminal.write(next.value);
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
      if (!closing) {
        addErrorToast(error);
      }
    });
  }

  function clearPendingResize(): void {
    if (resizeTimer !== undefined) {
      clearTimeout(resizeTimer);
      resizeTimer = undefined;
    }
    pendingResize = undefined;
    resizeBurstStartedAt = undefined;
  }

  function teardown(): Promise<void> {
    teardownTask ??= closeAttachment(true);
    return teardownTask;
  }

  async function closeAttachment(markClosing: boolean): Promise<void> {
    closing = markClosing;
    clearPendingResize();
    const activeAttachment = attachment;
    await releaseAttachment(activeAttachment, true);
    if (markClosing) {
      status = "Detached";
    }
  }

  async function releaseEndedAttachment(activeAttachment: SessionAttachment | undefined): Promise<void> {
    await releaseAttachment(activeAttachment, false);
  }

  async function releaseAttachment(
    activeAttachment: SessionAttachment | undefined,
    reportDetachError: boolean,
  ): Promise<void> {
    if (activeAttachment === undefined || attachment !== activeAttachment) {
      return;
    }
    writer?.releaseLock();
    writer = undefined;
    attachment = undefined;
    clearPendingResize();
    try {
      await activeAttachment.detach();
    } catch (error: unknown) {
      if (reportDetachError && !closing) {
        addErrorToast(error);
      }
    }
  }
</script>

<div class="embedded-terminal">
  <div class="terminal-status-line" data-testid="terminal-status" role="status" aria-live="polite">
    <span class:status-attached={status === "Attached"} class="terminal-status-dot" aria-hidden="true"></span>
    <span>{status}</span>
    {#if failed}
      <button type="button" onclick={() => void retry()}>Retry</button>
    {/if}
  </div>
  <div
    class="terminal-host"
    bind:this={terminalElement}
    data-testid="terminal"
    aria-label={`Terminal for ${sessionId} on ${host}`}
  ></div>
</div>
