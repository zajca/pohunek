import { afterAll, describe, expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  NODE_EXECUTABLE_ENV,
  NodeExecutableError,
  resolveNodeExecutable,
} from "../node-executable";

const roots: string[] = [];

afterAll(async () => {
  await Promise.all(roots.map((root) => rm(root, { recursive: true, force: true })));
});

describe("Node executable discovery", () => {
  test("a configured absolute executable wins over PATH", async () => {
    const configured = await fakeNode("configured");
    const onPath = await fakeNode("on-path");
    const found = await resolveNodeExecutable({
      env: { [NODE_EXECUTABLE_ENV]: configured, PATH: join(onPath, "..") },
      platform: "linux",
    });
    expect(found).toBe(configured);
  });

  test("a configured value must be an absolute executable file", async () => {
    await expectFailure({ [NODE_EXECUTABLE_ENV]: "node" }, "must be an absolute path");
    await expectFailure(
      { [NODE_EXECUTABLE_ENV]: join(await tempRoot(), "missing") },
      "not an executable file",
    );
    const directory = await tempRoot();
    await expectFailure({ [NODE_EXECUTABLE_ENV]: directory }, "not an executable file");
    const plain = join(directory, "node");
    await writeFile(plain, "", { mode: 0o600 });
    await expectFailure({ [NODE_EXECUTABLE_ENV]: plain }, "not an executable file");
  });

  test("the first executable node on PATH wins and relative entries are skipped", async () => {
    const first = await fakeNode("first");
    const second = await fakeNode("second");
    const found = await resolveNodeExecutable({
      env: { PATH: ["relative/bin", ".", dirOf(first), dirOf(second)].join(":") },
      platform: "linux",
    });
    expect(found).toBe(first);
  });

  test("macOS falls back to the install prefixes when PATH has no node", async () => {
    const prefix = await fakeNode("prefix");
    const found = await resolveNodeExecutable({
      env: { PATH: "/nonexistent-minimal" },
      platform: "darwin",
      fallbackDirectories: [dirOf(prefix)],
    });
    expect(found).toBe(prefix);
  });

  test("no fallback directories means only PATH is searched", async () => {
    await fakeNode("unsearched-prefix");
    await expectFailure({ PATH: "/nonexistent-minimal" }, "any fallback directory");
  });

  test("a missing node names both the search and the override", async () => {
    await expectFailure({ PATH: "/nonexistent-minimal" }, NODE_EXECUTABLE_ENV);
  });
});

async function expectFailure(env: NodeJS.ProcessEnv, message: string): Promise<void> {
  let failure: unknown;
  try {
    await resolveNodeExecutable({ env, platform: "linux", fallbackDirectories: [] });
  } catch (error: unknown) {
    failure = error;
  }
  expect(failure).toBeInstanceOf(NodeExecutableError);
  expect((failure as NodeExecutableError).message).toContain(message);
}

async function tempRoot(): Promise<string> {
  const root = await realpath(await mkdtemp(join(tmpdir(), "pohunek-node-discovery-")));
  roots.push(root);
  return root;
}

async function fakeNode(name: string): Promise<string> {
  const directory = join(await tempRoot(), name);
  await mkdir(directory);
  const program = join(directory, "node");
  await writeFile(program, "#!/bin/sh\nexit 0\n");
  await chmod(program, 0o755);
  return program;
}

function dirOf(program: string): string {
  return program.slice(0, program.lastIndexOf("/"));
}
