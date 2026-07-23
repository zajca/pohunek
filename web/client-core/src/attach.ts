import { attachRawWs, type RawStream } from "@pohunek/sdk/browser";
import type { Methods } from "@pohunek/protocol";

export interface AttachCaller {
  call<K extends "session.attach" | "session.detach">(
    host: string,
    method: K,
    params: Methods[K]["params"],
  ): Promise<Methods[K]["output"]>;
}

export interface SessionAttachment {
  readonly host: string;
  readonly sessionId: string;
  readonly streamId: string;
  readonly stream: RawStream;
  detach(): Promise<Methods["session.detach"]["output"]>;
}

export async function attachSession(
  baseUrl: string,
  host: string,
  sessionId: string,
  caller: AttachCaller,
): Promise<SessionAttachment> {
  const attached = await caller.call(host, "session.attach", { session_id: sessionId });
  let stream: RawStream;
  try {
    stream = await attachRawWs(baseUrl, host, attached.stream_id);
  } catch (error: unknown) {
    try {
      await caller.call(host, "session.detach", { stream_id: attached.stream_id });
    } catch {
      // Preserve the raw attach failure; cleanup cannot make the stream usable.
    }
    throw error;
  }

  let detachTask: Promise<Methods["session.detach"]["output"]> | undefined;
  const detach = (): Promise<Methods["session.detach"]["output"]> => {
    detachTask ??= detachAndClose(caller, host, attached.stream_id, stream);
    return detachTask;
  };
  return {
    host,
    sessionId,
    streamId: attached.stream_id,
    stream,
    detach,
  };
}

async function detachAndClose(
  caller: AttachCaller,
  host: string,
  streamId: string,
  stream: RawStream,
): Promise<Methods["session.detach"]["output"]> {
  try {
    return await caller.call(host, "session.detach", { stream_id: streamId });
  } finally {
    await stream.close();
  }
}
