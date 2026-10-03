import { MAX_CONTROL_LINE_BYTES } from "@pohunek/protocol";
import { ClientError } from "./error";

const encoder = new TextEncoder();

export function encodeControlLine(line: string): Uint8Array {
  const bytes = encoder.encode(line);
  if (bytes.byteLength > MAX_CONTROL_LINE_BYTES) {
    throw ClientError.framing("control line exceeded maximum length");
  }

  const framed = new Uint8Array(bytes.byteLength + 1);
  framed.set(bytes, 0);
  framed[bytes.byteLength] = 0x0a;
  return framed;
}

export async function* readControlLines(chunks: AsyncIterable<Uint8Array>): AsyncGenerator<string> {
  const pending: number[] = [];
  for await (const chunk of chunks) {
    for (const byte of chunk) {
      if (byte === 0x0a) {
        yield decodeLine(pending);
        pending.length = 0;
        continue;
      }

      pending.push(byte);
      if (pending.length > MAX_CONTROL_LINE_BYTES) {
        throw ClientError.framing("control line exceeded maximum length");
      }
    }
  }

  if (pending.length > 0) {
    yield decodeLine(pending);
  }
}

function decodeLine(bytes: number[]): string {
  const lineBytes = bytes.at(-1) === 0x0d ? bytes.slice(0, -1) : bytes;
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(Uint8Array.from(lineBytes));
  } catch (error: unknown) {
    throw ClientError.framing(`invalid utf-8 control line: ${messageFromUnknown(error)}`);
  }
}

function messageFromUnknown(source: unknown): string {
  if (source instanceof Error) {
    return source.message;
  }
  return String(source);
}
