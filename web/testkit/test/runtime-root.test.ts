import { afterAll, describe, expect, test } from "bun:test";
import { rm, stat } from "node:fs/promises";
import { MACOS_FIXTURE_ROOT_PARENT, createFixtureRoot } from "@pohunek/testkit";

const roots: string[] = [];

afterAll(async () => {
  await Promise.all(roots.map((root) => rm(root, { recursive: true, force: true })));
});

describe("fixture roots", () => {
  test("are private, canonical and leave room for the daemon socket", async () => {
    const root = await createFixtureRoot("pk-root-");
    roots.push(root);
    const info = await stat(root);
    expect(info.isDirectory()).toBe(true);
    expect(info.mode & 0o777).toBe(0o700);
    expect(`${root}/runtime/pohunek/daemon.sock`.length <= 103).toBe(true);
    if (process.platform === "darwin") {
      expect(root.startsWith(`${MACOS_FIXTURE_ROOT_PARENT}/`)).toBe(true);
    }
  });

  test("a root that cannot fit the socket path fails with the cause", async () => {
    let message = "";
    try {
      const root = await createFixtureRoot("p".repeat(100));
      roots.push(root);
    } catch (error: unknown) {
      message = error instanceof Error ? error.message : String(error);
    }
    expect(message.includes("103-byte limit")).toBe(true);
  });
});
