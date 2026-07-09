export type RelayBindAddrErrorKind = "forbidden" | "notNetbird";

export interface ValidateRelayBindAddrOptions {
  readonly allowLoopback?: boolean;
}

export class RelayBindAddrError extends Error {
  public override readonly name = "RelayBindAddrError";
  public readonly kind: RelayBindAddrErrorKind;
  public readonly ip: string;

  public constructor(kind: RelayBindAddrErrorKind, ip: string) {
    super(messageForKind(kind, ip));
    this.kind = kind;
    this.ip = ip;
  }
}

// NetBird allocates peers from the CGNAT block 100.64.0.0/10. The relay must
// fail closed and bind only to that interface unless the caller explicitly asks
// for loopback in local tests/development.
const NETBIRD_IPV4_FIRST_OCTET = 100;
const NETBIRD_IPV4_SECOND_OCTET_MIN = 64;
const NETBIRD_IPV4_SECOND_OCTET_MAX = 127;
const IPV4_OCTET_COUNT = 4;
const IPV4_OCTET_MIN = 0;
const IPV4_OCTET_MAX = 255;
const LOOPBACK_IPV4_FIRST_OCTET = 127;

export function validateRelayBindAddr(
  ip: string,
  options: ValidateRelayBindAddrOptions = {},
): void {
  const parsed = parseIp(ip);
  if (parsed.kind === "ipv4") {
    if (isUnspecifiedIpv4(parsed.octets) || isLoopbackIpv4(parsed.octets)) {
      if (options.allowLoopback === true && isLoopbackIpv4(parsed.octets)) {
        return;
      }
      throw new RelayBindAddrError("forbidden", ip);
    }

    if (isNetbirdIpv4(parsed.octets)) {
      return;
    }

    throw new RelayBindAddrError("notNetbird", ip);
  }

  if (parsed.kind === "ipv6Unspecified") {
    throw new RelayBindAddrError("forbidden", ip);
  }
  if (parsed.kind === "ipv6Loopback") {
    if (options.allowLoopback === true) {
      return;
    }
    throw new RelayBindAddrError("forbidden", ip);
  }

  throw new RelayBindAddrError("notNetbird", ip);
}

export function isNetbirdIp(ip: string): boolean {
  const parsed = parseIp(ip);
  return parsed.kind === "ipv4" && isNetbirdIpv4(parsed.octets);
}

type ParsedIp =
  | { kind: "ipv4"; octets: readonly [number, number, number, number] }
  | { kind: "ipv6Loopback" }
  | { kind: "ipv6Unspecified" }
  | { kind: "other" };

function parseIp(ip: string): ParsedIp {
  const ipv4 = parseIpv4(ip);
  if (ipv4 !== undefined) {
    return { kind: "ipv4", octets: ipv4 };
  }

  if (ip === "::") {
    return { kind: "ipv6Unspecified" };
  }
  if (ip === "::1") {
    return { kind: "ipv6Loopback" };
  }

  return { kind: "other" };
}

function parseIpv4(ip: string): readonly [number, number, number, number] | undefined {
  const parts = ip.split(".");
  if (parts.length !== IPV4_OCTET_COUNT) {
    return undefined;
  }

  const octets = parts.map((part) => parseIpv4Octet(part));
  if (octets.some((octet) => octet === undefined)) {
    return undefined;
  }

  return octets as [number, number, number, number];
}

function parseIpv4Octet(part: string): number | undefined {
  if (!/^\d+$/.test(part)) {
    return undefined;
  }

  const value = Number(part);
  if (!Number.isInteger(value) || value < IPV4_OCTET_MIN || value > IPV4_OCTET_MAX) {
    return undefined;
  }

  return value;
}

function isUnspecifiedIpv4(octets: readonly [number, number, number, number]): boolean {
  return octets.every((octet) => octet === 0);
}

function isLoopbackIpv4(octets: readonly [number, number, number, number]): boolean {
  return octets[0] === LOOPBACK_IPV4_FIRST_OCTET;
}

function isNetbirdIpv4(octets: readonly [number, number, number, number]): boolean {
  return (
    octets[0] === NETBIRD_IPV4_FIRST_OCTET &&
    octets[1] >= NETBIRD_IPV4_SECOND_OCTET_MIN &&
    octets[1] <= NETBIRD_IPV4_SECOND_OCTET_MAX
  );
}

function messageForKind(kind: RelayBindAddrErrorKind, ip: string): string {
  if (kind === "forbidden") {
    return `bind address ${ip} is unspecified/loopback and must never be used unless loopback fallback is explicit`;
  }
  return `bind address ${ip} is not inside the NetBird range 100.64.0.0/10`;
}
