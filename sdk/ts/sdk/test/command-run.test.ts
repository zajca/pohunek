import { afterEach, describe, expect, test } from "bun:test";
import { MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_TIMEOUT_MS, PROTOCOL_VERSION } from "@pohunek/protocol";
import { ClientError, connectLocal, connectTcp, type Client } from "@pohunek/sdk";
import {
  createFixtureRootSync,
  startFixtureDaemon,
  type FixtureCommand,
  type FixtureDaemonHandle,
} from "@pohunek/testkit";
import { rmSync } from "node:fs";
import { errResponseLine, okResponseLine, requestIdFromLine, startTcpDaemon, type MockDaemon, type ScriptStep } from "./mock-daemon";

/**
 * `runCommand` against the fixture daemon and a mock daemon. The fixture runs
 * no process; the daemon-side behavior is covered by the daemon suites.
 */

const PACKAGE = "means.fleet-state";
const COMMAND = "host-state";
const OUTPUT = "line one\nzeichen: äö\n";
const HOST = "netbird:command-run";

const open: Array<{ close(): Promise<void> }> = [];

afterEach(async () => {
  while (open.length > 0) {
    await open.pop()?.close();
  }
});

let root: string | undefined;
let counter = 0;

function socketPath(): string {
  if (root === undefined) {
    root = createFixtureRootSync("pk-cmd-");
    const created = root;
    process.on("exit", (): void => {
      rmSync(created, { recursive: true, force: true });
    });
  }
  counter += 1;
  return `${root}/c-${counter}.sock`;
}

async function fixture(
  commands: readonly FixtureCommand[] | undefined,
  requestTimeoutMs?: number,
): Promise<{ daemon: FixtureDaemonHandle; client: Client }> {
  const path = socketPath();
  const daemon = await startFixtureDaemon({
    listen: { unixSocketPath: path },
    ...(commands === undefined ? {} : { commands }),
  });
  open.push(daemon);
  const client = await connectLocal(path, requestTimeoutMs === undefined ? undefined : { requestTimeoutMs });
  open.push(client);
  return { daemon, client };
}

async function rejection(promise: Promise<unknown>): Promise<ClientError> {
  try {
    await promise;
  } catch (error: unknown) {
    expect(error).toBeInstanceOf(ClientError);
    return error as ClientError;
  }
  throw new Error("expected the call to reject with ClientError");
}

describe("runCommand against the fixture daemon", () => {
  test("returns the identity, digest and verbatim output", async () => {
    const { client } = await fixture([{ package: PACKAGE, command: COMMAND, output: OUTPUT }]);
    const result = await client.runCommand({ package: PACKAGE, command: COMMAND });
    expect(result.package).toEqual({ id: PACKAGE, version: "1.0.0" });
    expect(result.command).toBe(COMMAND);
    expect(result.output).toBe(OUTPUT);
    expect(result.digest.startsWith("sha256:")).toBe(true);
  });

  test("Client.call routes command.run through the same path", async () => {
    const { client } = await fixture([{ package: PACKAGE, command: COMMAND, output: OUTPUT }]);
    const result = await client.call("command.run", { package: PACKAGE, command: COMMAND, timeout_ms: 5_000 });
    expect(result.output).toBe(OUTPUT);
  });

  test("a reply held past the default request timeout succeeds within the command timeout", async () => {
    let release = (): void => {};
    const hold = new Promise<void>((resolve) => {
      release = resolve;
    });
    // timing-allowed: #1690 the request deadline under test is shorter than the held reply
    const { client } = await fixture([{ package: PACKAGE, command: COMMAND, output: OUTPUT, hold }], 20);
    const running = client.runCommand({ package: PACKAGE, command: COMMAND, timeout_ms: 5_000 });
    // timing-allowed: #1690 the reply is released after the default request deadline has passed
    setTimeout(release, 80);
    expect((await running).output).toBe(OUTPUT);
  });

  test("an unconfigured package is not installed, a known package with an unknown command is not declared", async () => {
    const { client } = await fixture([{ package: PACKAGE, command: COMMAND, output: OUTPUT }]);
    const missing = await rejection(client.runCommand({ package: "means.other", command: COMMAND }));
    expect(missing.code).toBe("command_package_not_installed");
    const undeclared = await rejection(client.runCommand({ package: PACKAGE, command: "other" }));
    expect(undeclared.code).toBe("command_not_declared");
  });

  test("a timeout or output cap that is not a nonzero u32 is refused before sending", async () => {
    const { client } = await fixture([{ package: PACKAGE, command: COMMAND, output: OUTPUT }]);
    for (const bad of [Number.NaN, Number.POSITIVE_INFINITY, 0, -1, 1.5]) {
      for (const field of ["timeout_ms", "max_output_bytes"] as const) {
        const error = await rejection(client.runCommand({ package: PACKAGE, command: COMMAND, [field]: bad }));
        expect(error.code).toBe("bad_request");
      }
    }
  });

  test("each command_* error keeps its code and details", async () => {
    const codes = [
      "command_package_not_installed",
      "command_not_declared",
      "command_package_disabled",
      "command_not_selected",
      "command_not_exposed",
      "command_busy",
      "command_timed_out",
      "command_output_too_large",
      "command_output_not_utf8",
      "command_unavailable",
    ];
    for (const code of codes) {
      const { client } = await fixture([{
        package: PACKAGE,
        command: COMMAND,
        error: { class: "runtime", code, msg: "fixed text" },
      }]);
      const error = await rejection(client.runCommand({ package: PACKAGE, command: COMMAND }));
      expect(error.kind).toBe("protocol");
      expect(error.code).toBe(code);
      expect(error.errorClass).toBe("runtime");
      expect(error.details).toBeUndefined();
    }
  });

  test("command_failed carries the process exit details", async () => {
    for (const exit of [{ exit_code: 3 }, { signal: 9 }]) {
      const { client } = await fixture([{
        package: PACKAGE,
        command: COMMAND,
        error: { class: "runtime", code: "command_failed", msg: "fixed text", details: { process_exit: exit } },
      }]);
      const error = await rejection(client.runCommand({ package: PACKAGE, command: COMMAND }));
      expect(error.code).toBe("command_failed");
      expect(error.details).toEqual({ process_exit: exit });
      expect(error.toProtocolError().details).toEqual({ process_exit: exit });
    }
  });

  test("details cannot be changed through the returned copy", async () => {
    const { client } = await fixture([{
      package: PACKAGE,
      command: COMMAND,
      error: { class: "runtime", code: "command_failed", msg: "fixed text", details: { process_exit: { exit_code: 3 } } },
    }]);
    const error = await rejection(client.runCommand({ package: PACKAGE, command: COMMAND }));
    const copy = error.details;
    if (copy?.process_exit !== undefined) {
      copy.process_exit.exit_code = 99;
    }
    expect(error.details).toEqual({ process_exit: { exit_code: 3 } });
  });

  test("a fixture without the method fails with command_unsupported", async () => {
    const { client } = await fixture(undefined);
    const error = await rejection(client.runCommand({ package: PACKAGE, command: COMMAND }));
    expect(error.kind).toBe("commandUnsupported");
    expect(error.code).toBe("command_unsupported");
    expect(error.errorClass).toBe("daemon");
  });

  test("an unknown request field is not forwarded", async () => {
    const { client } = await fixture([{ package: PACKAGE, command: COMMAND, output: OUTPUT }]);
    const smuggled = { package: PACKAGE, command: COMMAND, args: ["--x"] } as unknown as Parameters<Client["runCommand"]>[0];
    expect((await client.runCommand(smuggled)).output).toBe(OUTPUT);
  });
});

describe("runCommand against a mock daemon", () => {
  async function mock(step: ScriptStep): Promise<{ daemon: MockDaemon; client: Client }> {
    // The version probe precedes the request and must leave the connection open.
    const probe: ScriptStep = {
      kind: "gate",
      ready: Promise.resolve(),
      line: (line) => okResponseLine(requestIdFromLine(line), {
        status: "ok",
        daemon_version: "0.0.0-test",
        protocol_version: PROTOCOL_VERSION,
      }),
    };
    const daemon = await startTcpDaemon([probe, step]);
    open.push(daemon);
    if (daemon.endpoint.kind !== "tcp") {
      throw new Error("the mock daemon needs a TCP endpoint");
    }
    const client = await connectTcp(HOST, { host: daemon.endpoint.host, port: daemon.endpoint.port });
    open.push(client);
    return { daemon, client };
  }

  test("a method_not_found answer maps to command_unsupported with the host", async () => {
    const { client } = await mock({
      kind: "reply",
      line: (line) => errResponseLine(requestIdFromLine(line), {
        class: "daemon",
        code: "method_not_found",
        msg: "unknown control method: command.run",
      }),
    });
    const error = await rejection(client.runCommand({ package: PACKAGE, command: COMMAND }));
    expect(error.kind).toBe("commandUnsupported");
    expect(error.toProtocolError().msg).toContain(HOST);
  });

  test("the request carries only the declared fields", async () => {
    const { daemon, client } = await mock({
      kind: "reply",
      line: (line) => okResponseLine(requestIdFromLine(line), {
        package: { id: PACKAGE, version: "1.0.0" },
        command: COMMAND,
        digest: `sha256:${"0".repeat(64)}`,
        output: OUTPUT,
        duration_ms: 1,
      }),
    });
    await client.runCommand({ package: PACKAGE, command: COMMAND, max_output_bytes: 1024 });
    expect((JSON.parse(await daemon.nextRequest()) as { method: string }).method).toBe("daemon.health");
    const request = JSON.parse(await daemon.nextRequest()) as { method: string; params: unknown };
    expect(request.method).toBe("command.run");
    expect(request.params).toEqual({ package: PACKAGE, command: COMMAND, max_output_bytes: 1024 });
  });

  test("the widened command budget does not leak onto the shared connection", async () => {
    let releaseCommand = (): void => {};
    const commandHeld = new Promise<void>((resolve) => {
      releaseCommand = resolve;
    });
    let releaseHealth = (): void => {};
    const healthHeld = new Promise<void>((resolve) => {
      releaseHealth = resolve;
    });
    const health = (ready: Promise<void>): ScriptStep => ({
      kind: "gate",
      ready,
      line: (line) => okResponseLine(requestIdFromLine(line), {
        status: "ok",
        daemon_version: "0.0.0-test",
        protocol_version: PROTOCOL_VERSION,
      }),
    });
    const daemon = await startTcpDaemon([
      health(Promise.resolve()),
      {
        kind: "gate",
        ready: commandHeld,
        line: (line) => okResponseLine(requestIdFromLine(line), {
          package: { id: PACKAGE, version: "1.0.0" },
          command: COMMAND,
          digest: `sha256:${"0".repeat(64)}`,
          output: OUTPUT,
          duration_ms: 1,
        }),
      },
      health(healthHeld),
    ]);
    open.push(daemon);
    if (daemon.endpoint.kind !== "tcp") {
      throw new Error("the mock daemon needs a TCP endpoint");
    }
    // timing-allowed: #1690 the default request deadline under test is shorter than the held replies
    const client = await connectTcp(HOST, { host: daemon.endpoint.host, port: daemon.endpoint.port }, { requestTimeoutMs: 20 });
    open.push(client);

    const running = client.runCommand({ package: PACKAGE, command: COMMAND, timeout_ms: 5_000 });
    // timing-allowed: #1690 the reply is released after the default request deadline has passed
    setTimeout(releaseCommand, 80);
    expect((await running).output).toBe(OUTPUT);

    // A non-command reply held past the default deadline still times out.
    const timedOut = await rejection(client.call("daemon.health", null));
    expect(timedOut.kind).toBe("requestTimeout");
    releaseHealth();
  });

  test("the ceiling constant is the protocol ceiling", () => {
    expect(MAX_COMMAND_TIMEOUT_MS).toBe(60_000);
    expect(MAX_COMMAND_OUTPUT_BYTES).toBe(131_072);
  });
});
