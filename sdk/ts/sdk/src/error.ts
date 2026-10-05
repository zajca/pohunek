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
  RequestTimeout: "request_timeout",
  Io: "io_error",
  Json: "json_error",
  InputWaitContract: "session_input_wait_contract_mismatch",
  VersionMismatch: "version_mismatch",
  DaemonProtocolTooOld: "daemon_protocol_too_old",
  VersionTranslationFailed: "version_translation_failed",
} as const;

export type ClientErrorKind =
  | "daemonUnreachable"
  | "framing"
  | "protocol"
  | "hostUnreachable"
  | "remoteDaemonUnavailable"
  | "requestTimeout"
  | "remoteProtocol"
  | "io"
  | "json"
  | "inputWaitContract"
  | "versionMismatch"
  | "daemonProtocolTooOld"
  | "versionTranslation";

/** Facts of a method that a daemon's older protocol version never defined. */
export interface DaemonProtocolTooOldDetail {
  /** Route of the remote host, or `undefined` for the local daemon. */
  readonly host: string | undefined;
  readonly method: string;
  /** Protocol version the daemon negotiated for the connection. */
  readonly daemonVersion: ProtocolVersion;
  /** Protocol version that defines the method. */
  readonly requiredVersion: ProtocolVersion;
}

export class ClientError extends Error {
  public override readonly name = "ClientError";
  public readonly kind: ClientErrorKind;
  public readonly errorClass: ErrorClass;
  public readonly code: string;
  public readonly source: unknown;
  /** Set for `daemonProtocolTooOld` errors. */
  public readonly tooOld: DaemonProtocolTooOldDetail | undefined;

  private readonly structured: ProtocolError;

  private constructor(
    kind: ClientErrorKind,
    message: string,
    structured: ProtocolError,
    source?: unknown,
    tooOld?: DaemonProtocolTooOldDetail,
  ) {
    super(message);
    this.tooOld = tooOld;
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

  public static requestTimeout(remoteHost: string | undefined, timeoutMs: number): ClientError {
    const target = remoteHost === undefined ? "the local daemon" : `host '${remoteHost}'`;
    const msg = `timed out after ${timeoutMs}ms waiting for a response from ${target}`;
    return new ClientError(
      "requestTimeout",
      msg,
      protocolError(
        ClientErrorClass.Transport,
        ClientErrorCode.RequestTimeout,
        msg,
        "the request may have completed; reconcile daemon state before retrying a mutation",
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

  public static inputWaitContract(detail: string): ClientError {
    const msg = `invalid session.input wait response: ${detail}`;
    return new ClientError(
      "inputWaitContract",
      msg,
      protocolError(
        ClientErrorClass.Daemon,
        ClientErrorCode.InputWaitContract,
        msg,
        "delivery outcome is unknown; do not retry blindly; upgrade the daemon and client together, then inspect the session before deciding whether to resend",
      ),
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

  /** A method the daemon's negotiated older protocol version never defined. */
  public static daemonProtocolTooOld(detail: DaemonProtocolTooOldDetail): ClientError {
    const target = daemonTarget(detail.host);
    const msg = `${target} runs protocol ${detail.daemonVersion}, but \`${detail.method}\` needs protocol ${detail.requiredVersion}`;
    return new ClientError(
      "daemonProtocolTooOld",
      msg,
      protocolError(
        ClientErrorClass.Daemon,
        ClientErrorCode.DaemonProtocolTooOld,
        msg,
        `upgrade pohunek on ${target} to a release that speaks protocol ${detail.requiredVersion}`,
      ),
      undefined,
      { ...detail },
    );
  }

  /** A payload that could not be translated for the daemon's older version. */
  public static versionTranslation(
    host: string | undefined,
    daemonVersion: ProtocolVersion,
    source: unknown,
  ): ClientError {
    const target = daemonTarget(host);
    const msg = `a payload cannot be translated for ${target}, which runs protocol ${daemonVersion}`;
    return new ClientError(
      "versionTranslation",
      msg,
      protocolError(
        ClientErrorClass.Daemon,
        ClientErrorCode.VersionTranslationFailed,
        msg,
        `upgrade pohunek on ${target} so both sides speak the same protocol version`,
      ),
      source,
    );
  }

  public toProtocolError(): ProtocolError {
    return cloneProtocolError(this.structured);
  }

  public recoverHint(): string | undefined {
    return this.structured.recover;
  }
}

function daemonTarget(host: string | undefined): string {
  return host === undefined ? "the local daemon" : `host '${host}'`;
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
