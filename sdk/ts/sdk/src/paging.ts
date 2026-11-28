import {
  LIST_WALK_MAX_BYTES,
  LIST_WALK_MAX_ITEMS,
  PROJECT_LIST_PAGE_SIZE,
  SESSION_LIST_PAGE_SIZE,
  type ProjectInfo,
  type SessionInfo,
} from "@pohunek/protocol";
import { ClientError } from "./error";

/** Asks a daemon for one page of a list method. Mirrors `paging::PageSource`. */
export interface PageSource<T> {
  /** One request for at most `limit` records after `cursor`. */
  fetch(limit: number, cursor: string | undefined): Promise<T[]>;
}

/**
 * What distinguishes one paged list method from another. Mirrors
 * `paging::PageWalk`.
 */
export interface PageWalk<T> {
  /** Records one page asks for. */
  readonly pageSize: number;
  /** The code of the daemon's "page does not fit one control line" error. */
  readonly tooLargeCode: string;
  /** The client-raised code for a page that ends at or before its cursor. */
  readonly cursorRepeatedCode: string;
  /** The client-raised code for a walk past the item or byte bound. */
  readonly walkLimitCode: string;
  /** The method name, for messages. */
  readonly method: string;
  /** The cursor that continues the listing after this record (`Paged::cursor`). */
  readonly cursor: (record: T) => string;
}

/** The `session.list` walk. Mirrors `paging::SESSION_LIST`. */
export const SESSION_LIST: PageWalk<SessionInfo> = {
  pageSize: SESSION_LIST_PAGE_SIZE,
  tooLargeCode: "session_list_too_large",
  cursorRepeatedCode: "session_list_cursor_repeated",
  walkLimitCode: "session_list_walk_limit",
  method: "session.list",
  cursor: (session) => session.id,
};

/** The `project.list` walk. Mirrors `paging::PROJECT_LIST`. */
export const PROJECT_LIST: PageWalk<ProjectInfo> = {
  pageSize: PROJECT_LIST_PAGE_SIZE,
  tooLargeCode: "project_list_too_large",
  cursorRepeatedCode: "project_list_cursor_repeated",
  walkLimitCode: "project_list_walk_limit",
  method: "project.list",
  cursor: (project) => project.id,
};

const UTF8_ENCODER = new TextEncoder();

/**
 * Rust `str::cmp` order: UTF-8 byte order, which equals code-point order.
 * JavaScript's `<` compares UTF-16 code units and differs for a supplementary
 * character against a `U+E000..U+FFFF` one, so the walk compares code points.
 */
export function compareStrings(a: string, b: string): number {
  const left = a[Symbol.iterator]();
  const right = b[Symbol.iterator]();
  for (;;) {
    const leftStep = left.next();
    const rightStep = right.next();
    if (leftStep.done === true && rightStep.done === true) {
      return 0;
    }
    if (leftStep.done === true) {
      return -1;
    }
    if (rightStep.done === true) {
      return 1;
    }
    const leftPoint = leftStep.value.codePointAt(0) ?? 0;
    const rightPoint = rightStep.value.codePointAt(0) ?? 0;
    if (leftPoint !== rightPoint) {
      return leftPoint < rightPoint ? -1 : 1;
    }
  }
}

/** Mirrors `paging::client_error`. */
function clientError(code: string, msg: string, hint: string): ClientError {
  return ClientError.protocol({ class: "daemon", code, msg, recover: hint });
}

/** Mirrors the `Err(ClientError::Protocol(source) | ClientError::RemoteProtocol { .. })` arm. */
function isProtocolErrorWithCode(error: unknown, code: string): boolean {
  return error instanceof ClientError
    && (error.kind === "protocol" || error.kind === "remoteProtocol")
    && error.code === code;
}

/** The byte length `serde_json::to_vec` would produce, mirrored for the walk bound. */
function utf8Bytes(value: string): number {
  return UTF8_ENCODER.encode(value).byteLength;
}

/**
 * Collects every record of the method by following cursors, asking `source`
 * for one page at a time. Mirrors `paging::every_page`.
 *
 * A page the daemon reports too large is asked again at half the size, down to
 * one record. A daemon that predates paging ignores `limit` and `cursor`, and
 * answers the whole current set to every request: an answer longer than the
 * page size, a first page that is not in ascending cursor order, or one that
 * starts at or before the cursor (a paging daemon never answers a record at or
 * before it) is that set and is returned as it is.
 *
 * @throws The error of `source`, or a client-raised error when a page ends at
 * or before its cursor or the walk passes `LIST_WALK_MAX_ITEMS` or
 * `LIST_WALK_MAX_BYTES`.
 */
export async function everyPage<T>(spec: PageWalk<T>, source: PageSource<T>): Promise<T[]> {
  let pageSize = spec.pageSize;
  let cursor: string | undefined;
  const records: T[] = [];
  let bytes = 0;
  for (;;) {
    let page: T[];
    try {
      page = await source.fetch(pageSize, cursor);
    } catch (error: unknown) {
      // `paging::every_page`: a page at or below one record is never halved again.
      if (pageSize > 1 && isProtocolErrorWithCode(error, spec.tooLargeCode)) {
        pageSize = Math.floor(pageSize / 2);
        continue;
      }
      throw error;
    }
    const pageLen = page.length;
    if (records.length === 0 && pageLen > pageSize) {
      return page;
    }
    if (pageLen === 0) {
      break;
    }
    const first = spec.cursor(page[0]!);
    const last = spec.cursor(page[pageLen - 1]!);
    // A paging daemon answers ascending cursor order; the first page of a
    // daemon that predates paging is in its own order.
    if (cursor === undefined) {
      for (let index = 0; index + 1 < pageLen; index += 1) {
        if (compareStrings(spec.cursor(page[index]!), spec.cursor(page[index + 1]!)) >= 0) {
          return page;
        }
      }
    }
    if (cursor !== undefined) {
      if (compareStrings(first, cursor) <= 0) {
        return page;
      }
      if (compareStrings(last, cursor) <= 0) {
        throw clientError(
          spec.cursorRepeatedCode,
          `the daemon answered a ${spec.method} page that does not follow its cursor`,
          "upgrade the daemon and client together",
        );
      }
    }
    bytes += utf8Bytes(JSON.stringify(page));
    records.push(...page);
    if (records.length > LIST_WALK_MAX_ITEMS || bytes > LIST_WALK_MAX_BYTES) {
      throw clientError(
        spec.walkLimitCode,
        `the ${spec.method} walk exceeded the records or bytes a client collects`,
        "narrow the listing with filters",
      );
    }
    if (pageLen < pageSize) {
      break;
    }
    cursor = last;
  }
  return records;
}
