import type {
  ErrorClass,
  ProtocolError,
  ProtocolVersion,
  ProtocolVersionRange,
} from "@pohunek/protocol";

export const ClientErrorClass = {
  Configuration: "configuration",
  Daemon: "daemon",
  Transport: "transport",
  Runtime: "runtime",
  Discovery: "discovery",
} as const satisfies Record<string, ErrorClass>;

export const ClientErrorCode = {
  DaemonUnreachable: "daemon_unreachable",
  Framing: "framing",
  HostUnreachable: "host_unreachable",
  RemoteDaemonUnavailable: "remote_daemon_unavailable",
  Io: "io_error",
  Json: "json_error",
  VersionMismatch: "version_mismatch",
} as const;

export type ClientErrorKind =
  | "daemonUnreachable"
  | "framing"
  | "protocol"
  | "hostUnreachable"
  | "remoteDaemonUnavailable"
  | "remoteProtocol"
  | "io"
  | "json"
  | "versionMismatch";

export class ClientError extends Error {
  public override readonly name = "ClientError";
  public readonly kind: ClientErrorKind;
  public readonly errorClass: ErrorClass;
  public readonly code: string;
  public readonly source: unknown;

  private readonly structured: ProtocolError;

  private constructor(kind: ClientErrorKind, message: string, structured: ProtocolError, source?: unknown) {
    super(message);
    this.kind = kind;
    this.errorClass = structured.class;
    this.code = structured.code;
    this.structured = cloneProtocolError(structured);
    this.source = source;
  }

  public static daemonUnreachable(socketPath: string, source: unknown): ClientError {
    const detail = messageFromUnknown(source);
    const msg = `cannot reach the daemon at ${socketPath}: ${detail}`;
    return new ClientError(
      "daemonUnreachable",
      msg,
      protocolError(
        ClientErrorClass.Daemon,
        ClientErrorCode.DaemonUnreachable,
        msg,
        "start the daemon with `pohunek daemon start`",
      ),
      source,
    );
  }

  public static framing(message: string): ClientError {
    return new ClientError(
      "framing",
      `protocol framing error: ${message}`,
      protocolError(
        ClientErrorClass.Transport,
        ClientErrorCode.Framing,
        `protocol framing error: ${message}`,
      ),
    );
  }

  public static protocol(source: ProtocolError): ClientError {
    return new ClientError("protocol", `daemon error: ${source.msg}`, source, source);
  }

  public static remoteProtocol(host: string, source: ProtocolError): ClientError {
    const structured = cloneProtocolError(source);
    structured.msg = `host '${host}': ${source.msg}`;
    return new ClientError("remoteProtocol", structured.msg, structured, source);
  }

  public static hostUnreachable(host: string, source: unknown): ClientError {
    const detail = messageFromUnknown(source);
    const msg = `could not open a NetBird connection to host '${host}': ${detail}`;
    return new ClientError(
      "hostUnreachable",
      msg,
      protocolError(
        ClientErrorClass.Transport,
        ClientErrorCode.HostUnreachable,
        msg,
        "check that the host is online and its pohunek daemon is running",
      ),
      source,
    );
  }

  public static remoteDaemonUnavailable(host: string): ClientError {
    const msg = `connected to host '${host}' but no compatible pohunek daemon answered`;
    return new ClientError(
      "remoteDaemonUnavailable",
      msg,
      protocolError(
        ClientErrorClass.Daemon,
        ClientErrorCode.RemoteDaemonUnavailable,
        msg,
        "ensure a matching pohunek daemon is running on the host",
      ),
    );
  }

  public static io(source: unknown): ClientError {
    const msg = `io error: ${messageFromUnknown(source)}`;
    return new ClientError(
      "io",
      msg,
      protocolError(ClientErrorClass.Runtime, ClientErrorCode.Io, msg),
      source,
    );
  }

  public static json(source: unknown): ClientError {
    const msg = `json error: ${messageFromUnknown(source)}`;
    return new ClientError(
      "json",
      msg,
      protocolError(ClientErrorClass.Daemon, ClientErrorCode.Json, msg),
      source,
    );
  }

  public static versionMismatch(
    clientVersion: ProtocolVersion | ProtocolVersionRange,
    daemonVersion: ProtocolVersion,
  ): ClientError {
    const clientLabel = typeof clientVersion === "number"
      ? String(clientVersion)
      : `${clientVersion.minimum}..=${clientVersion.maximum}`;
    const msg = `client protocol version ${clientLabel} is incompatible with daemon protocol version ${daemonVersion}`;
    return new ClientError(
      "versionMismatch",
      msg,
      protocolError(
        ClientErrorClass.Daemon,
        ClientErrorCode.VersionMismatch,
        msg,
        "upgrade the older side so both speak the same protocol version",
      ),
    );
  }

  public toProtocolError(): ProtocolError {
    return cloneProtocolError(this.structured);
  }

  public recoverHint(): string | undefined {
    return this.structured.recover;
  }
}

function protocolError(
  errorClass: ErrorClass,
  code: string,
  msg: string,
  recover?: string,
): ProtocolError {
  if (recover === undefined) {
    return { class: errorClass, code, msg };
  }
  return { class: errorClass, code, msg, recover };
}

function cloneProtocolError(error: ProtocolError): ProtocolError {
  return protocolError(error.class, error.code, error.msg, error.recover);
}

function messageFromUnknown(source: unknown): string {
  if (source instanceof Error) {
    return source.message;
  }
  if (typeof source === "string") {
    return source;
  }
  return String(source);
}
