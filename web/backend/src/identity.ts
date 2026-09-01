import { Buffer } from "node:buffer";

const PEER_IDENTITY_PREFIX = "peer~";
const FQDN_IDENTITY_PREFIX = "fqdn~";

export function externalPeerSelector(peerId: string): string {
  return encodeExternalIdentity(PEER_IDENTITY_PREFIX, peerId);
}

export function externalFqdnSelector(fqdn: string): string {
  return encodeExternalIdentity(FQDN_IDENTITY_PREFIX, fqdn);
}

function encodeExternalIdentity(prefix: string, value: string): string {
  if (value.length === 0) {
    throw new Error("external peer identity must be non-empty");
  }
  return `${prefix}${Buffer.from(value, "utf8").toString("base64url")}`;
}
