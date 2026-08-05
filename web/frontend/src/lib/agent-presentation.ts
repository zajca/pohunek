import type { AgentKind, AgentRuntime } from "@pohunek/protocol";

const KNOWN_AGENT_LABELS: Readonly<Record<string, string>> = {
  shell: "Shell",
  codex: "Codex",
  claude: "Claude Code",
  hermes: "Hermes",
};

/** Returns a neutral, forward-compatible label for a protocol agent kind. */
export function agentKindLabel(agent: AgentKind): string {
  return KNOWN_AGENT_LABELS[agent] ?? `Unknown agent (${agent})`;
}

/** Labels a profile while keeping its resolved base kind understandable. */
export function agentProfileLabel(profile: string, base: AgentKind): string {
  const baseLabel = agentKindLabel(base);
  return profile === base ? baseLabel : `${profile} · ${baseLabel}`;
}

/** Labels a session agent while tolerating legacy peers that omitted its base. */
export function sessionAgentLabel(profile: string, base?: AgentKind): string {
  return base === undefined ? profile : agentProfileLabel(profile, base);
}

/** Returns whether a daemon-advertised agent base is safe to launch from this client. */
export function isLaunchableAgentKind(agent: AgentKind): boolean {
  return Object.hasOwn(KNOWN_AGENT_LABELS, agent);
}

/** Returns whether both persisted and active agent bases have known mutation semantics. */
export function hasKnownSessionAgentBases(base: AgentKind, activeBase?: AgentKind): boolean {
  return isLaunchableAgentKind(base)
    && (activeBase === undefined || isLaunchableAgentKind(activeBase));
}

/** Allows attach unless a persisted or active base is explicitly unknown. */
export function hasAttachableSessionAgentBases(base?: AgentKind, activeBase?: AgentKind): boolean {
  return (base === undefined || isLaunchableAgentKind(base))
    && (activeBase === undefined || isLaunchableAgentKind(activeBase));
}

/** Presents inventory state without exposing command probe output or binary paths. */
export function agentRuntimeStatus(runtime: AgentRuntime | undefined): string {
  if (runtime?.available !== true) {
    return "missing";
  }
  if (runtime.supported === false) {
    return "installed (unsupported)";
  }
  if (runtime.supported === true) {
    return "installed (supported)";
  }
  return "installed";
}

/** Produces a neutral inventory label for built-in and profile-backed runtimes. */
export function agentRuntimeLabel(agent: string, runtime: AgentRuntime | undefined): string {
  const base = runtime?.agent_base;
  const profileLabel = base === undefined
    ? (isLaunchableAgentKind(agent) ? agentKindLabel(agent) : agent)
    : agentProfileLabel(agent, base);
  return `${profileLabel} — ${agentRuntimeStatus(runtime)}`;
}

/** Allows mutations only for an available, supported runtime with a known base. */
export function isLaunchableRuntime(agent: string, runtime: AgentRuntime | undefined): boolean {
  if (runtime?.available !== true) {
    return false;
  }
  const base = runtime.agent_base;
  if (base !== undefined && !isLaunchableAgentKind(base)) {
    return false;
  }
  if (base === "hermes" || (base === undefined && agent === "hermes")) {
    return runtime.supported === true;
  }
  return runtime.supported !== false;
}
