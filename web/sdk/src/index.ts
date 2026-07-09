export { Client, nextRequestId } from "./client";
export {
  attachRaw,
  attachRawLocal,
  attachRawTcp,
  attachRawWs,
  connectRawLocal,
  connectRawTcp,
  connectRawWs,
} from "./attach";
export type { RawStream } from "./attach";
export { ClientError, ClientErrorClass, ClientErrorCode } from "./error";
export type { ClientErrorKind } from "./error";
export {
  decodeResponse,
  isErrResponse,
  isEvent,
  isOkResponse,
  isRequest,
} from "./envelope";
export type { ErrResponse, Event, OkResponse, Request, Response } from "./envelope";
export {
  DEFAULT_CONNECT_TIMEOUT_MS,
  DEFAULT_REQUEST_TIMEOUT_MS,
  resolveConnectOptions,
} from "./transport";
export type {
  ConnectOptions,
  ControlChannel,
  RawDuplex,
  ResolvedConnectOptions,
  Transport,
} from "./transport";
export { SocketTransport } from "./transport-socket";
export { WsTransport } from "./transport-ws";
export { Subscription, decodeProtocolEvent } from "./subscription";
export type { CatchAllEvent } from "./subscription";
export * from "@pohunek/protocol";
