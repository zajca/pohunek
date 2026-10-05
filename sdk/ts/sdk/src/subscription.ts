import { EVENT_NAMES, PROTOCOL_VERSION, type ProtocolEvent, type ProtocolVersion } from "@pohunek/protocol";
import { CompatError, upgradeEventPayload } from "./compat";
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

  /**
   * Returns the next event line, or `null` when the daemon closes.
   *
   * A connection that selected the previous protocol version yields each event
   * translated to the current shape and re-serialized.
   */
  public async nextLine(): Promise<string | null> {
    const line = await this.nextRawLine();
    if (line === null || this.selectedVersion === PROTOCOL_VERSION) {
      return line;
    }
    return JSON.stringify(this.decode(line));
  }

  /** Returns the next event in the current shape, or `null` when the daemon closes. */
  public async nextEvent(): Promise<ProtocolEvent | CatchAllEvent | null> {
    const line = await this.nextRawLine();
    if (line === null) {
      return null;
    }
    return this.decode(line);
  }

  private async nextRawLine(): Promise<string | null> {
    try {
      const next = await this.lines.next();
      return next.done === true ? null : next.value;
    } catch (error: unknown) {
      throw this.mapReadError(error);
    }
  }

  private decode(line: string): ProtocolEvent | CatchAllEvent {
    const event = decodeProtocolEvent(this.parseEventLine(line));
    if (event.v !== this.selectedVersion) {
      throw ClientError.versionMismatch(this.selectedVersion, event.v);
    }
    if (event.v === PROTOCOL_VERSION) {
      return event;
    }
    try {
      const payload: Record<string, unknown> = { ...event };
      delete payload["v"];
      delete payload["event"];
      delete payload["id"];
      const { event: name, id } = event;
      const upgraded = upgradeEventPayload(event.v, name, payload);
      if (typeof upgraded !== "object" || upgraded === null) {
        throw ClientError.json("translated event payload is not an object");
      }
      return { ...upgraded, v: PROTOCOL_VERSION, event: name, ...(id === undefined ? {} : { id }) };
    } catch (error: unknown) {
      if (error instanceof CompatError) {
        throw ClientError.versionTranslation(this.remoteHost, event.v, error);
      }
      throw error;
    }
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
