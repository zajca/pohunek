import type {
  ErrorClass,
  ErrorDetails,
  ProtocolError,
  ProtocolVersion,
  ProtocolVersionRange,
} from "@pohunek/protocol";

/**
 * The class of an error as a client observes it. A class that a newer daemon
 * defines and this build does not reads as `"unknown"`, so the daemon's `code`,
 * `msg` and `recover` stay available. The generated `ErrorClass` type lists
 * only the classes this build defines and is unchanged.
 */
export type ObservedErrorClass = ErrorClass | "unknown";

/** A `ProtocolError` whose class may be `"unknown"`. */
export type ObservedProtocolError = Omit<ProtocolError, "class"> & {
  class: ObservedErrorClass;
};

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
  SessionResumeOutcomeUnknown: "session_resume_outcome_unknown",
  SessionForkOutcomeUnknown: "session_fork_outcome_unknown",
  SessionStopOutcomeUnknown: "session_stop_outcome_unknown",
  SessionRemoveOutcomeUnknown: "session_remove_outcome_unknown",
  Io: "io_error",
  Json: "json_error",
  InputWaitContract: "session_input_wait_contract_mismatch",
  VersionMismatch: "version_mismatch",
  DaemonProtocolTooOld: "daemon_protocol_too_old",
  VersionTranslationFailed: "version_translation_failed",
  SessionNewSecretEnvUnsupported: "session_new_secret_env_unsupported",
  SessionNewSecretEnvRelayRefused: "session_new_secret_env_relay_refused",
  CommandUnsupported: "command_unsupported",
  PackageInstallCommandsUnsupported: "package_install_commands_unsupported",
} as const;

export type ClientErrorKind =
  | "daemonUnreachable"
  | "framing"
  | "protocol"
  | "hostUnreachable"
  | "remoteDaemonUnavailable"
  | "requestTimeout"
  | "sessionResumeOutcomeUnknown"
  | "sessionForkOutcomeUnknown"
  | "sessionStopOutcomeUnknown"
  | "sessionRemoveOutcomeUnknown"
  | "remoteProtocol"
  | "io"
  | "json"
  | "inputWaitContract"
  | "versionMismatch"
  | "daemonProtocolTooOld"
  | "versionTranslation"
  | "sessionNewSecretEnvUnsupported"
  | "sessionNewSecretEnvRelayRefused"
  | "commandUnsupported"
  | "packageInstallCommandsUnsupported";

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
  public readonly errorClass: ObservedErrorClass;
  public readonly code: string;
  public readonly source: unknown;
  /** Set for `daemonProtocolTooOld` errors. */
  public readonly tooOld: DaemonProtocolTooOldDetail | undefined;

  private readonly structured: ObservedProtocolError;

  private constructor(
    kind: ClientErrorKind,
    message: string,
    structured: ObservedProtocolError,
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

  public static protocol(source: ObservedProtocolError): ClientError {
    return new ClientError("protocol", `daemon error: ${source.msg}`, source, source);
  }

  public static remoteProtocol(host: string, source: ObservedProtocolError): ClientError {
    const structured = cloneProtocolError(source);
    structured.msg = `host '${host}': ${source.msg}`;
    return new ClientError("remoteProtocol", structured.msg, structured, source);
  }

  public static hostUnreachable(host: string, source: unknown): ClientError {
    const detail = messageFromUnknown(source);
    const msg = `could not open an overlay connection to host '${host}': ${detail}`;
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

  public static requestTimeout(
    remoteHost: string | undefined,
    timeoutMs: number,
    phase?: "version probe",
  ): ClientError {
    const target = remoteHost === undefined ? "the local daemon" : `host '${remoteHost}'`;
    const subject = phase === undefined ? "a response" : `a response to the ${phase}`;
    const msg = `timed out after ${timeoutMs}ms waiting for ${subject} from ${target}`;
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

  /**
   * A lifecycle request whose response deadline elapsed. The daemon may still
   * complete it, so the outcome is unknown.
   */
  public static lifecycleOutcomeUnknown(
    operation: LifecycleOperation,
    remoteHost: string | undefined,
    timeoutMs: number,
  ): ClientError {
    const detail = LIFECYCLE_OUTCOME_UNKNOWN[operation];
    const target = remoteHost === undefined ? "the local daemon" : `host '${remoteHost}'`;
    const msg = `timed out after ${timeoutMs}ms waiting for ${target} to ${detail.action}`;
    return new ClientError(
      detail.kind,
      msg,
      protocolError(ClientErrorClass.Transport, detail.code, msg, detail.recover),
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

  /**
   * The daemon does not advertise `session_new_secret_env_supported`, so it
   * would drop `secret_env` silently. The message names no variable and no value.
   */
  public static sessionNewSecretEnvUnsupported(host: string | undefined): ClientError {
    const target = daemonTarget(host);
    const msg = `${target} predates secret_env and would drop it silently; nothing was sent`;
    return new ClientError(
      "sessionNewSecretEnvUnsupported",
      msg,
      protocolError(
        ClientErrorClass.Daemon,
        ClientErrorCode.SessionNewSecretEnvUnsupported,
        msg,
        "upgrade the daemon, then retry",
      ),
    );
  }

  /**
   * The daemon does not serve `command.run`: it answered `method_not_found`, or
   * its negotiated protocol version never defined the method.
   */
  public static commandUnsupported(host: string | undefined): ClientError {
    const target = daemonTarget(host);
    const msg = `${target} does not support command.run`;
    return new ClientError(
      "commandUnsupported",
      msg,
      protocolError(
        ClientErrorClass.Daemon,
        ClientErrorCode.CommandUnsupported,
        msg,
        `upgrade pohunek on ${target} to a release that supports package commands`,
      ),
    );
  }

  /**
   * The daemon does not advertise `package_install_commands_supported`, so it
   * would reject `accept_commands`; nothing was sent.
   */
  public static packageInstallCommandsUnsupported(host: string | undefined): ClientError {
    const target = daemonTarget(host);
    const msg = `${target} predates package commands and rejects accept_commands; nothing was sent`;
    return new ClientError(
      "packageInstallCommandsUnsupported",
      msg,
      protocolError(
        ClientErrorClass.Daemon,
        ClientErrorCode.PackageInstallCommandsUnsupported,
        msg,
        "upgrade the daemon, then retry",
      ),
    );
  }

  /** `secret_env` must not pass through the relay, which must never see raw secrets. */
  public static sessionNewSecretEnvRelayRefused(host: string | undefined): ClientError {
    const target = daemonTarget(host);
    const msg = `secret_env is not sent through the relay to ${target}`;
    return new ClientError(
      "sessionNewSecretEnvRelayRefused",
      msg,
      protocolError(
        ClientErrorClass.Configuration,
        ClientErrorCode.SessionNewSecretEnvRelayRefused,
        msg,
        "connect to the host directly over a local socket or an overlay TCP connection",
      ),
    );
  }

  /** Structured detail the daemon attached to the error, for example the exit status of a failed command. */
  public get details(): ErrorDetails | undefined {
    return this.structured.details === undefined ? undefined : structuredClone(this.structured.details);
  }

  public toProtocolError(): ObservedProtocolError {
    return cloneProtocolError(this.structured);
  }

  public recoverHint(): string | undefined {
    return this.structured.recover;
  }
}

/** Lifecycle requests whose elapsed deadline leaves the outcome unknown. */
export type LifecycleOperation = "resume" | "fork" | "stop" | "remove";

const LIFECYCLE_OUTCOME_UNKNOWN: Record<
  LifecycleOperation,
  { kind: ClientErrorKind; code: string; action: string; recover: string }
> = {
  resume: {
    kind: "sessionResumeOutcomeUnknown",
    code: ClientErrorCode.SessionResumeOutcomeUnknown,
    action: "resume the session",
    recover: "the resume may still complete: run `pohunek session inspect <session>` and read its runtime state before retrying",
  },
  fork: {
    kind: "sessionForkOutcomeUnknown",
    code: ClientErrorCode.SessionForkOutcomeUnknown,
    action: "fork the session",
    recover: "the fork may still complete and a retried fork creates a second child session: run `pohunek session list` and look for the new session before retrying",
  },
  stop: {
    kind: "sessionStopOutcomeUnknown",
    code: ClientErrorCode.SessionStopOutcomeUnknown,
    action: "stop the session",
    recover: "the stop may still complete: run `pohunek session inspect <session>` and read its state; repeating a stop is harmless",
  },
  remove: {
    kind: "sessionRemoveOutcomeUnknown",
    code: ClientErrorCode.SessionRemoveOutcomeUnknown,
    action: "remove the session",
    recover: "the removal may still complete: run `pohunek session list` to see whether the session is gone before retrying",
  },
};

function daemonTarget(host: string | undefined): string {
  return host === undefined ? "the local daemon" : `host '${host}'`;
}

function protocolError(
  errorClass: ObservedErrorClass,
  code: string,
  msg: string,
  recover?: string,
  details?: ErrorDetails,
): ObservedProtocolError {
  const error: ObservedProtocolError = { class: errorClass, code, msg };
  if (recover !== undefined) {
    error.recover = recover;
  }
  if (details !== undefined) {
    error.details = structuredClone(details);
  }
  return error;
}

function cloneProtocolError(error: ObservedProtocolError): ObservedProtocolError {
  return protocolError(error.class, error.code, error.msg, error.recover, error.details);
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
