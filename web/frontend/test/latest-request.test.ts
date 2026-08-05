import { describe, expect, test } from "bun:test";
import { LatestRequest } from "../src/lib/latest-request";

describe("latest async request guard", () => {
  test("rejects an old host response after switching during await", async () => {
    const guard = new LatestRequest();
    let currentHost = "host-a";
    const oldRequest = guard.begin(currentHost);
    const oldResponse = deferred<string>();
    const applyOld = oldResponse.promise.then((value) =>
      guard.isCurrent(oldRequest, currentHost) ? value : undefined,
    );

    currentHost = "host-b";
    guard.invalidate();
    oldResponse.resolve("policy-a");

    expect(await applyOld).toBeUndefined();
  });

  test("rejects old session output and accepts the new session response", async () => {
    const guard = new LatestRequest();
    let currentSession = "host-a:session-1";
    const oldRequest = guard.begin(currentSession);
    const oldResponse = deferred<string>();
    const applyOld = oldResponse.promise.then((value) =>
      guard.isCurrent(oldRequest, currentSession) ? value : undefined,
    );

    currentSession = "host-a:session-2";
    guard.invalidate();
    const newRequest = guard.begin(currentSession);
    oldResponse.resolve("output-1");

    expect(await applyOld).toBeUndefined();
    expect(guard.isCurrent(newRequest, currentSession)).toBe(true);
  });

  test("keeps a newer same-host policy edit when an older save resolves", async () => {
    const guard = new LatestRequest();
    const host = "host-a";
    const saveRequest = guard.begin(host);
    const saveResponse = deferred<{ readonly system: boolean }>();
    let localPolicy = { system: true };
    const applySave = saveResponse.promise.then((value): void => {
      if (guard.isCurrent(saveRequest, host)) {
        localPolicy = value;
      }
    });

    guard.invalidate();
    localPolicy = { system: false };
    saveResponse.resolve({ system: true });
    await applySave;

    expect(localPolicy.system).toBe(false);
  });
});

function deferred<T>(): { readonly promise: Promise<T>; readonly resolve: (value: T) => void } {
  let resolvePromise: ((value: T) => void) | undefined;
  const promise = new Promise<T>((resolve) => {
    resolvePromise = resolve;
  });
  return {
    promise,
    resolve: (value: T): void => {
      resolvePromise?.(value);
    },
  };
}
