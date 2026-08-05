import { EVENT_NAMES, type ProtocolEvent, type ProtocolVersion } from "@pohunek/protocol";
import { ClientError } from "./error";
import { isEvent } from "./envelope";
import type { ControlChannel } from "./transport";

export type CatchAllEvent = {
  v: ProtocolVersion;
  event: string;
  id?: string;
} & Record<string, unknown>;

export class Subscription {
  private readonly lines: AsyncIterator<string>;
  private readonly selectedVersion: ProtocolVersion;
  private readonly remoteHost: string | undefined;

  public constructor(channel: ControlChannel, selectedVersion: ProtocolVersion, remoteHost?: string) {
    this.lines = channel.lines[Symbol.asyncIterator]();
    this.selectedVersion = selectedVersion;
    this.remoteHost = remoteHost;
  }

  public async nextLine(): Promise<string | null> {
    try {
      const next = await this.lines.next();
      return next.done === true ? null : next.value;
    } catch (error: unknown) {
      throw this.mapReadError(error);
    }
  }

  public async nextEvent(): Promise<ProtocolEvent | CatchAllEvent | null> {
    const line = await this.nextLine();
    if (line === null) {
      return null;
    }

    const value = this.parseEventLine(line);
    const event = decodeProtocolEvent(value);
    if (event.v !== this.selectedVersion) {
      throw ClientError.versionMismatch(this.selectedVersion, event.v);
    }
    return event;
  }

  private parseEventLine(line: string): unknown {
    try {
      return JSON.parse(line) as unknown;
    } catch (error: unknown) {
      throw this.unparseableError(error);
    }
  }

  private mapReadError(error: unknown): ClientError {
    if (this.remoteHost !== undefined) {
      return ClientError.remoteDaemonUnavailable(this.remoteHost);
    }
    if (error instanceof ClientError) {
      return error;
    }
    return ClientError.io(error);
  }

  private unparseableError(error: unknown): ClientError {
    if (this.remoteHost !== undefined) {
      return ClientError.remoteDaemonUnavailable(this.remoteHost);
    }
    return ClientError.json(error);
  }
}

export function decodeProtocolEvent(value: unknown): ProtocolEvent | CatchAllEvent {
  if (!isEvent(value)) {
    throw ClientError.json("invalid event envelope");
  }
  if (isKnownEventName(value.event)) {
    return value;
  }
  return value;
}

function isKnownEventName(event: string): boolean {
  return (EVENT_NAMES as readonly string[]).includes(event);
}
