import { describe, expect, test } from "bun:test";
import { RelayBindAddrError, validateRelayBindAddr } from "@pohunek/backend";

describe("validateRelayBindAddr", () => {
  test("accepts NetBird CGNAT IPv4 addresses", () => {
    expect(validateRelayBindAddr("100.64.0.1")).toBeUndefined();
    expect(validateRelayBindAddr("100.92.10.20")).toBeUndefined();
    expect(validateRelayBindAddr("100.127.255.255")).toBeUndefined();
  });

  test("rejects addresses just outside the NetBird range as not-netbird", () => {
    expectBindError("100.63.255.255", "notNetbird");
    expectBindError("100.128.0.0", "notNetbird");
  });

  test("rejects unspecified and loopback addresses as forbidden by default", () => {
    expectBindError("0.0.0.0", "forbidden");
    expectBindError("127.0.0.1", "forbidden");
    expectBindError("::", "forbidden");
    expectBindError("::1", "forbidden");
  });

  test("allows loopback only when the local fallback is explicit", () => {
    expect(validateRelayBindAddr("127.0.0.1", { allowLoopback: true })).toBeUndefined();
    expect(validateRelayBindAddr("::1", { allowLoopback: true })).toBeUndefined();
    expectBindError("0.0.0.0", "forbidden", { allowLoopback: true });
    expectBindError("::", "forbidden", { allowLoopback: true });
  });

  test("rejects RFC1918 and public addresses as not-netbird", () => {
    expectBindError("10.0.0.1", "notNetbird");
    expectBindError("192.168.1.1", "notNetbird");
    expectBindError("172.16.0.1", "notNetbird");
    expectBindError("8.8.8.8", "notNetbird");
  });

  test("rejects non-loopback IPv6 as not-netbird", () => {
    expectBindError("2001:db8::1", "notNetbird");
    expectBindError("::ffff:100.64.0.1", "notNetbird");
  });
});

function expectBindError(
  ip: string,
  expectedKind: "forbidden" | "notNetbird",
  options?: { allowLoopback?: boolean },
): void {
  try {
    validateRelayBindAddr(ip, options);
  } catch (error: unknown) {
    expect(error).toBeInstanceOf(RelayBindAddrError);
    const bindError = error as RelayBindAddrError;
    expect(bindError.kind).toBe(expectedKind);
    expect(bindError.ip).toBe(ip);
    return;
  }
  throw new Error(`expected ${ip} to be rejected as ${expectedKind}`);
}
