import type {
  HostSnapshot,
  HostsSnapshot,
} from "@pohunek/client-core";
import { describe, expect, test } from "bun:test";
import { selectPrimaryHosts } from "../src/lib/host-presentation";

describe("primary host presentation", () => {
  test("keeps connected daemons and version mismatches while hiding inactive discovery noise", () => {
    const hosts: HostsSnapshot = {
      local: host("local", "reachable_daemon", { kind: "connected" }),
      sleeping: host("sleeping", "unreachable", { kind: "error", reason: "offline" }),
      ephemeral: host("ephemeral", "candidate", { kind: "connecting" }),
      staleDaemon: host("stale-daemon", "reachable_daemon", { kind: "error", reason: "offline" }),
      incompatible: host("incompatible", "version_mismatch", { kind: "version_mismatch", theirs: 99 }),
    };

    const primary = selectPrimaryHosts(hosts);

    expect(primary.reachableDaemons.map((entry) => entry.host)).toEqual(["local", "stale-daemon"]);
    expect(primary.versionMismatches.map((entry) => entry.host)).toEqual(["incompatible"]);
  });

  test("sorts switchable hosts by name", () => {
    const hosts: HostsSnapshot = {
      zebra: host("zebra", "reachable_daemon", { kind: "connected" }),
      alpha: host("alpha", "reachable_daemon", { kind: "connected" }),
    };

    expect(selectPrimaryHosts(hosts).reachableDaemons.map((entry) => entry.host)).toEqual(["alpha", "zebra"]);
  });
});

function host(
  name: string,
  reachability: HostSnapshot["reachability"],
  connection: HostSnapshot["connection"],
): HostSnapshot {
  return { host: name, reachability, connection };
}
