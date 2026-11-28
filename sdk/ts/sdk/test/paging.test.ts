import { afterEach, describe, expect, test } from "bun:test";
import {
  PROJECT_LIST_PAGE_SIZE,
  SESSION_LIST_PAGE_SIZE,
  type ProjectInfo,
  type SessionInfo,
} from "@pohunek/protocol";
import { Client, ClientError, connectTcp } from "@pohunek/sdk";
import {
  errResponseLine,
  minimalSessionInfo,
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startTcpDaemon,
  type MockDaemon,
  type ScriptStep,
} from "./mock-daemon";

/**
 * `session.list` and `project.list` page over `cursor`, so a caller with many
 * records gets the whole set and never the daemon's `session_list_too_large` /
 * `project_list_too_large` refusal. The walk mirrors the Rust client
 * (`crates/client/src/paging.rs`): it also recognizes a daemon that predates
 * paging, which ignores `limit` and `cursor` and answers the whole set.
 */

const HOST = "netbird:paging-sdk";
const SETTLE_MS = 100;
// Equal to the suite hang guard: a lost reply fails the test instead of a short client deadline.
const REQUEST_TIMEOUT_MS = 120_000;

const PAGE = SESSION_LIST_PAGE_SIZE;

const open: Array<{ close(): Promise<void> }> = [];

afterEach(async () => {
  while (open.length > 0) {
    await open.pop()?.close();
  }
});

function track<T extends { close(): Promise<void> }>(resource: T): T {
  open.push(resource);
  return resource;
}

function sessionId(n: number): SessionInfo {
  return { ...minimalSessionInfo(), id: `s-${String(n).padStart(5, "0")}` };
}

/** `count` session ids in ascending order starting at `first`. */
function sessionIds(first: number, count: number): SessionInfo[] {
  return Array.from({ length: count }, (_, index) => sessionId(first + index));
}

function projectId(n: number): ProjectInfo {
  const id = `p-${String(n).padStart(5, "0")}`;
  return {
    id,
    label: id,
    repo_root: `/repos/${id}`,
    git_common_dir: `/repos/${id}/.git`,
    source: "auto",
    is_bare: false,
    added_at: "2026-11-26T00:00:00Z",
    last_used_at: "2026-11-26T00:00:00Z",
  };
}

/** `count` projects in ascending id order; the label is set separately. */
function projectIds(first: number, count: number): ProjectInfo[] {
  return Array.from({ length: count }, (_, index) => projectId(first + index));
}

/** The request's params object, or an empty one. */
function paramsOf(line: string): Record<string, unknown> {
  const params = parseRequestLine(line)["params"];
  return typeof params === "object" && params !== null ? (params as Record<string, unknown>) : {};
}

function pageLimit(line: string): number | undefined {
  const limit = paramsOf(line)["limit"];
  return typeof limit === "number" ? limit : undefined;
}

function pageCursor(line: string): string | undefined {
  const cursor = paramsOf(line)["cursor"];
  return typeof cursor === "string" ? cursor : undefined;
}

/** Answers without closing the connection, so the walk's next page reuses it. */
function keepOpen(line: (requestLine: string) => string): ScriptStep {
  return { kind: "gate", ready: Promise.resolve(), line };
}

/** A paging daemon: one page of `all` after the request's `cursor`. */
function pagedStep<T>(all: T[], id: (record: T) => string): ScriptStep {
  return keepOpen((requestLine) => {
    const params = paramsOf(requestLine);
    const limit = typeof params["limit"] === "number" ? params["limit"] : all.length;
    const cursor = typeof params["cursor"] === "string" ? params["cursor"] : undefined;
    const after = cursor === undefined
      ? 0
      : all.findIndex((record) => id(record) === cursor) + 1;
    return okResponseLine(requestIdFromLine(requestLine), all.slice(after, after + limit));
  });
}

/** A daemon that predates paging: the whole set, ignoring `limit` and `cursor`. */
function wholeStep<T>(all: T[]): ScriptStep {
  return keepOpen((requestLine) => okResponseLine(requestIdFromLine(requestLine), all));
}

function tooLargeStep(): ScriptStep {
  return keepOpen((requestLine) => errResponseLine(requestIdFromLine(requestLine), {
    class: "runtime",
    code: "session_list_too_large",
    msg: "the matching sessions exceed the public response budget",
  }));
}

function emptyStep(): ScriptStep {
  return keepOpen((requestLine) => okResponseLine(requestIdFromLine(requestLine), []));
}

async function connect(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("paging tests require a real TCP endpoint");
  }
  return connectTcp(
    HOST,
    { host: daemon.endpoint.host, port: daemon.endpoint.port },
    { requestTimeoutMs: REQUEST_TIMEOUT_MS },
  );
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

describe("session.list paging", () => {
  test("walks every page of a paging daemon", async () => {
    const all = sessionIds(0, PAGE + 50);
    const daemon = track(await startTcpDaemon([pagedStep(all, (s) => s.id), pagedStep(all, (s) => s.id)]));
    const client = await connect(daemon);

    const listed = await client.call("session.list", {});

    expect(listed.map((session) => session.id)).toEqual(all.map((session) => session.id));
    const first = await daemon.nextRequest();
    const second = await daemon.nextRequest();
    expect(pageLimit(first)).toBe(PAGE);
    expect(pageCursor(first)).toBeUndefined();
    expect(pageLimit(second)).toBe(PAGE);
    expect(pageCursor(second)).toBe(sessionId(PAGE - 1).id);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a caller's explicit limit is one request, not a walk", async () => {
    const all = sessionIds(0, PAGE + 50);
    const daemon = track(await startTcpDaemon([pagedStep(all, (s) => s.id)]));
    const client = await connect(daemon);

    const listed = await client.call("session.list", { limit: 5 });

    expect(listed.length).toBe(5);
    expect(pageLimit(await daemon.nextRequest())).toBe(5);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("recognizes an old daemon that answers more than one page at once", async () => {
    const all = sessionIds(0, PAGE + 50);
    const daemon = track(await startTcpDaemon([wholeStep(all)]));
    const client = await connect(daemon);

    const listed = await client.call("session.list", {});

    expect(listed.map((session) => session.id)).toEqual(all.map((session) => session.id));
    await daemon.nextRequest();
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("recognizes an old daemon by a first page that is not in ascending order", async () => {
    const all = [sessionId(2), sessionId(1), sessionId(0)];
    const daemon = track(await startTcpDaemon([wholeStep(all)]));
    const client = await connect(daemon);

    const listed = await client.call("session.list", {});

    expect(listed.map((session) => session.id)).toEqual(all.map((session) => session.id));
    await daemon.nextRequest();
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("an unchanged set of exactly one page is listed once", async () => {
    const all = sessionIds(0, PAGE);
    const daemon = track(await startTcpDaemon([wholeStep(all), wholeStep(all)]));
    const client = await connect(daemon);

    const listed = await client.call("session.list", {});

    expect(listed.map((session) => session.id)).toEqual(all.map((session) => session.id));
    await daemon.nextRequest();
    await daemon.nextRequest();
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("refuses a page that does not follow its cursor", async () => {
    const all = sessionIds(0, PAGE);
    const daemon = track(await startTcpDaemon([
      pagedStep(all, (s) => s.id),
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), [sessionId(300), sessionId(100)]) },
    ]));
    const client = await connect(daemon);

    const error = await rejection(client.call("session.list", {}));

    expect(error.kind).toBe("protocol");
    expect(error.code).toBe("session_list_cursor_repeated");
    await client.close();
  });

  test("halves the page size after session_list_too_large", async () => {
    const all = sessionIds(0, 1);
    // 200 -> 100 -> 50 -> 25 -> 12 -> 6 -> 3 -> 1: the seventh refusal is the
    // last halving, then the walk asks again at one record per page.
    const daemon = track(await startTcpDaemon([
      ...Array.from({ length: 7 }, tooLargeStep),
      pagedStep(all, (s) => s.id),
      emptyStep(),
    ]));
    const client = await connect(daemon);

    const listed = await client.call("session.list", {});

    expect(listed.map((session) => session.id)).toEqual(all.map((session) => session.id));
    const limits: number[] = [];
    for (let index = 0; index < 9; index += 1) {
      limits.push(pageLimit(await daemon.nextRequest()) ?? 0);
    }
    expect(limits).toEqual([200, 100, 50, 25, 12, 6, 3, 1, 1]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });
});

describe("project.list paging", () => {
  test("walks every page and reads the set ordered by label", async () => {
    const firstPage = projectIds(0, PROJECT_LIST_PAGE_SIZE);
    // The last record sorts before every id-ordered page, so the final order
    // proves the walked set is re-sorted by label, then id.
    const last = { ...projectId(PROJECT_LIST_PAGE_SIZE), label: "a-first" };
    const all = [...firstPage, last];
    const daemon = track(await startTcpDaemon([pagedStep(all, (p) => p.id), pagedStep(all, (p) => p.id)]));
    const client = await connect(daemon);

    const projects = await client.call("project.list", {});

    expect(projects.length).toBe(all.length);
    // Pages follow the id; the walked set reads by label, then id.
    expect(projects[0]?.id).toBe(last.id);
    await daemon.nextRequest();
    const second = await daemon.nextRequest();
    expect(pageCursor(second)).toBe(firstPage[firstPage.length - 1]?.id);
    await client.close();
  });
});
