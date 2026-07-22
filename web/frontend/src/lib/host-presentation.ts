import type {
  HostSnapshot,
  HostsSnapshot,
} from "@pohunek/client-core";

export interface PrimaryHosts {
  readonly reachableDaemons: readonly HostSnapshot[];
  readonly versionMismatches: readonly HostSnapshot[];
}

/** Selects the small set of hosts that carry an actionable shell status. */
export function selectPrimaryHosts(hosts: HostsSnapshot): PrimaryHosts {
  const entries = Object.values(hosts).sort((left, right) => left.host.localeCompare(right.host));
  return {
    reachableDaemons: entries.filter((host) => host.reachability === "reachable_daemon"),
    versionMismatches: entries.filter(
      (host) => host.reachability === "version_mismatch" || host.connection.kind === "version_mismatch",
    ),
  };
}
