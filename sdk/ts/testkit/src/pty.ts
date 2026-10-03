import { Buffer } from "node:buffer";
import type { Socket } from "node:net";
import type { SessionId } from "@pohunek/protocol";

const DEFAULT_PTY_READY_TEXT = "pty";
const DEFAULT_ECHO_INPUT = true;
const STREAM_ID_PREFIX = "a-testkit-";

const encoder = new TextEncoder();

export const DEFAULT_PTY_READY_BYTES = encoder.encode(DEFAULT_PTY_READY_TEXT);

export interface FixturePtyOptions {
  readonly readyBytes?: Uint8Array;
  readonly echoInput?: boolean;
}

export interface FixturePtyEvents {
  emitAttachOpened(sessionId: SessionId, streamId: string): void;
  emitAttachClosed(sessionId: SessionId, streamId: string): void;
}

export interface RedeemedPtyAttach {
  readonly active: ActivePtyAttach;
  readonly sessionId: SessionId;
}

interface PendingAttach {
  readonly sessionId: SessionId;
}

interface QueuedOutput {
  readonly bytes: Uint8Array;
}

export class ActivePtyAttach {
  public readonly streamId: string;
  public readonly sessionId: SessionId;

  private readonly socket: Socket;
  private readonly echoInput: boolean;
  private readonly events: FixturePtyEvents;
  private closed = false;

  public constructor(
    streamId: string,
    sessionId: SessionId,
    socket: Socket,
    echoInput: boolean,
    events: FixturePtyEvents,
  ) {
    this.streamId = streamId;
    this.sessionId = sessionId;
    this.socket = socket;
    this.echoInput = echoInput;
    this.events = events;
  }

  public writeInput(bytes: Uint8Array): void {
    if (bytes.byteLength === 0 || !this.echoInput) {
      return;
    }
    this.writeOutput(bytes);
  }

  public writeOutput(bytes: Uint8Array): void {
    if (this.closed || bytes.byteLength === 0) {
      return;
    }
    this.socket.write(Buffer.from(bytes));
  }

  public close(): void {
    if (this.closed) {
      return;
    }
    this.socket.destroy();
    this.finish();
  }

  public finish(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.events.emitAttachClosed(this.sessionId, this.streamId);
  }
}

export class FixturePtyRegistry {
  private readonly readyBytes: Uint8Array;
  private readonly echoInput: boolean;
  private readonly events: FixturePtyEvents;
  private readonly pending = new Map<string, PendingAttach>();
  private readonly active = new Map<string, ActivePtyAttach>();
  private readonly queuedOutput = new Map<SessionId, QueuedOutput[]>();
  private nextStreamId = 1;

  public constructor(events: FixturePtyEvents, options: FixturePtyOptions = {}) {
    this.events = events;
    this.readyBytes = options.readyBytes ?? DEFAULT_PTY_READY_BYTES;
    this.echoInput = options.echoInput ?? DEFAULT_ECHO_INPUT;
  }

  public mint(sessionId: SessionId): string {
    const streamId = `${STREAM_ID_PREFIX}${this.nextStreamId}`;
    this.nextStreamId += 1;
    this.pending.set(streamId, { sessionId });
    return streamId;
  }

  public redeem(streamId: string, socket: Socket, bufferedInput: Uint8Array): RedeemedPtyAttach | undefined {
    const pending = this.pending.get(streamId);
    if (pending === undefined) {
      return undefined;
    }
    this.pending.delete(streamId);

    const active = new ActivePtyAttach(streamId, pending.sessionId, socket, this.echoInput, this.events);
    this.active.set(streamId, active);
    socket.once("close", (): void => {
      this.finish(streamId);
    });
    socket.once("end", (): void => {
      this.finish(streamId);
    });

    this.events.emitAttachOpened(pending.sessionId, streamId);
    active.writeOutput(this.readyBytes);
    for (const queued of this.takeQueuedOutput(pending.sessionId)) {
      active.writeOutput(queued.bytes);
    }
    active.writeInput(bufferedInput);

    return { active, sessionId: pending.sessionId };
  }

  public detach(streamId: string): boolean {
    const active = this.active.get(streamId);
    if (active === undefined) {
      return false;
    }
    active.close();
    this.active.delete(streamId);
    return true;
  }

  public writeToSession(sessionId: SessionId, bytes: Uint8Array): number {
    let written = 0;
    for (const active of this.active.values()) {
      if (active.sessionId === sessionId) {
        active.writeOutput(bytes);
        written += 1;
      }
    }
    return written;
  }

  public queueOutput(sessionId: SessionId, bytes: Uint8Array): void {
    const queued = this.queuedOutput.get(sessionId) ?? [];
    queued.push({ bytes: copyBytes(bytes) });
    this.queuedOutput.set(sessionId, queued);
  }

  public closeSession(sessionId: SessionId): void {
    for (const [streamId, pending] of this.pending.entries()) {
      if (pending.sessionId === sessionId) {
        this.pending.delete(streamId);
      }
    }
    for (const [streamId, active] of this.active.entries()) {
      if (active.sessionId === sessionId) {
        active.close();
        this.active.delete(streamId);
      }
    }
    this.queuedOutput.delete(sessionId);
  }

  public closeAll(): void {
    this.pending.clear();
    this.queuedOutput.clear();
    for (const [streamId, active] of this.active.entries()) {
      active.close();
      this.active.delete(streamId);
    }
  }

  private finish(streamId: string): void {
    const active = this.active.get(streamId);
    if (active === undefined) {
      return;
    }
    this.active.delete(streamId);
    active.finish();
  }

  private takeQueuedOutput(sessionId: SessionId): QueuedOutput[] {
    const queued = this.queuedOutput.get(sessionId);
    if (queued === undefined) {
      return [];
    }
    this.queuedOutput.delete(sessionId);
    return queued;
  }
}

function copyBytes(bytes: Uint8Array): Uint8Array {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy;
}
