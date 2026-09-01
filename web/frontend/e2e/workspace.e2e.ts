import type { Locator, Page } from "@playwright/test";
import { connectTcp } from "../../sdk/src/node";
import {
  FIXTURE_LOCAL_HOST,
  FIXTURE_LOCAL_SESSION_ID,
  FIXTURE_LEGACY_BASELESS_SESSION_ID,
  FIXTURE_PEER_HOST,
  FIXTURE_PEER_SESSION_ID,
  FIXTURE_UNKNOWN_ACTIVE_SESSION_ID,
  FIXTURE_UNKNOWN_PERSISTED_SESSION_ID,
  type FixtureStackHandle,
} from "../../scripts/fixture-stack";
import { expect, test } from "./fixtures";

test("keeps every host in one session rail and promotes live blocked work", async ({ page, stack }) => {
  await page.goto(stack.backend.url);

  const localHost = hostMarker(page, FIXTURE_LOCAL_HOST);
  const peerHost = hostMarker(page, FIXTURE_PEER_HOST);
  await expect(localHost.locator("[data-connection]"))
    .toHaveAttribute("data-connection", "connected");
  await expect(peerHost.locator("[data-connection]"))
    .toHaveAttribute("data-connection", "connected");

  const localSession = sessionRow(page, FIXTURE_LOCAL_HOST, FIXTURE_LOCAL_SESSION_ID);
  const peerSession = sessionRow(page, FIXTURE_PEER_HOST, FIXTURE_PEER_SESSION_ID);
  await expect(localSession).toBeVisible();
  await expect(peerSession).toBeVisible();

  stack.local.scenario.setAgentState(FIXTURE_LOCAL_SESSION_ID, "blocked", "report");
  const badge = localSession.locator("[data-agent-state]");
  await expect(badge).toHaveAttribute("data-agent-state", "blocked");
  await expect(badge).toHaveAttribute("data-state-source", "report");
  await expect(page.getByRole("button", { name: "1 blocked" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Attention · 1" })).toBeVisible();
  await expect(page.getByTestId("session-row").first()).toHaveAttribute(
    "data-session-id",
    FIXTURE_LOCAL_SESSION_ID,
  );

  await page.getByRole("searchbox", { name: "Search sessions" }).fill(FIXTURE_PEER_HOST);
  await expect(peerSession).toBeVisible();
  await expect(localSession).toBeHidden();
});

test("creates on the chosen host, attaches immediately, and stops from the toolbar", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  await expect(hostMarker(page, FIXTURE_PEER_HOST).locator("[data-connection]"))
    .toHaveAttribute("data-connection", "connected");

  await page.getByRole("button", { name: "New session", exact: true }).first().click();
  const createDialog = page.getByRole("dialog", { name: "New session" });
  await createDialog.getByRole("combobox", { name: "Host", exact: true }).selectOption(FIXTURE_PEER_HOST);
  const agentSelect = createDialog.getByRole("combobox", { name: "Agent", exact: true });
  await expect(agentSelect).toBeEnabled();
  await agentSelect.selectOption("codex");
  await createDialog.getByRole("textbox", { name: /Name/ }).fill("Browser-created session");
  await expect(createDialog.getByTestId("terminal-size-probe")).toBeHidden();
  await createDialog.getByRole("button", { name: "Create and attach" }).click();

  await expect(createDialog).toBeHidden();
  await expect(page.getByRole("heading", { name: "Browser-created session" })).toBeVisible();
  await expect(page.getByTestId("terminal-status")).toContainText("Attached");
  await page.getByRole("button", { name: "Details" }).click();

  const inspector = page.getByRole("dialog", { name: "Browser-created session" });
  await expect(inspector.locator(":focus")).toHaveAttribute("aria-label", "Close session details");
  await expect(page.locator('button[aria-label="Close session details"][tabindex="-1"]')).toHaveCount(1);
  await page.keyboard.press("Shift+Tab");
  await expect(inspector.locator(":focus")).toHaveCount(1);
  await expect(inspector.getByTestId("session-detail")).toContainText(FIXTURE_PEER_HOST);
  await inspector.getByRole("button", { name: "Close session details" }).click();
  await page.getByRole("button", { name: "Stop", exact: true }).click();
  const confirmation = page.getByRole("dialog", { name: "Stop this session?" });
  await confirmation.getByRole("button", { name: "Stop session" }).click();
  await expect(page.getByRole("button", { name: "Resume", exact: true })).toBeVisible();
});

test("renders and launches Hermes with resume-only lifecycle capabilities", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  await page.getByRole("button", { name: "New session", exact: true }).first().click();

  const createDialog = page.getByRole("dialog", { name: "New session" });
  await createDialog.getByRole("combobox", { name: "Host", exact: true }).selectOption(FIXTURE_PEER_HOST);
  const agentSelect = createDialog.getByRole("combobox", { name: "Agent", exact: true });
  await expect(agentSelect.locator('option[value="hermes"]')).toHaveText("Hermes — installed (supported)");
  await agentSelect.selectOption("hermes");
  await createDialog.getByRole("textbox", { name: /Name/ }).fill("Browser Hermes session");
  await createDialog.getByRole("button", { name: "Create and attach" }).click();

  await expect(page.getByRole("heading", { name: "Browser Hermes session" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Fork unsupported", exact: true })).toBeDisabled();
  const hermesRow = page.getByTestId("session-row").filter({ hasText: "Browser Hermes session" });
  const sessionId = await hermesRow.getAttribute("data-session-id");
  if (sessionId === null) throw new Error("Hermes session row did not expose its session id");
  await reportNativeIdentity(stack, sessionId, "hermes", "browser-hermes-native");
  await page.getByRole("button", { name: "Stop", exact: true }).click();
  await page.getByRole("dialog", { name: "Stop this session?" }).getByRole("button", { name: "Stop session" }).click();
  await expect(page.getByRole("button", { name: "Resume", exact: true })).toBeVisible();
});

test("does not offer resume for fresh Hermes without a native reference", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  await page.getByRole("button", { name: "New session", exact: true }).first().click();

  const createDialog = page.getByRole("dialog", { name: "New session" });
  await createDialog.getByRole("combobox", { name: "Host", exact: true }).selectOption(FIXTURE_PEER_HOST);
  await createDialog.getByRole("combobox", { name: "Agent", exact: true }).selectOption("hermes");
  await createDialog.getByRole("textbox", { name: /Name/ }).fill("Fresh Hermes without identity");
  await createDialog.getByRole("button", { name: "Create and attach" }).click();

  await page.getByRole("button", { name: "Stop", exact: true }).click();
  await page.getByRole("dialog", { name: "Stop this session?" }).getByRole("button", { name: "Stop session" }).click();
  await expect(page.getByRole("button", { name: "Resume unavailable", exact: true })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Resume", exact: true })).toHaveCount(0);
});

test("keeps a known session with an unknown active agent presentation-only", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  const row = sessionRow(page, FIXTURE_LOCAL_HOST, FIXTURE_UNKNOWN_ACTIVE_SESSION_ID);
  await expect(row).toContainText("Unknown active agent session");
  await row.click();
  await expect(page.getByTestId("session-summary")).toContainText("future-profile · Unknown agent (future-agent)");
  for (const name of ["Rename", "Stop", "Resume", "Fork", "Fork unsupported", "Remove"]) {
    await expect(page.getByRole("button", { name, exact: true })).toHaveCount(0);
  }
});

test("keeps an explicitly unknown persisted agent presentation-only", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  const row = sessionRow(page, FIXTURE_LOCAL_HOST, FIXTURE_UNKNOWN_PERSISTED_SESSION_ID);
  await row.click();
  await expect(page.getByTestId("session-summary")).toContainText("Unknown agent (future-agent)");
  await expect(page.getByTestId("terminal-status")).toHaveCount(0);
});

test("keeps a legacy session without agent_base attachable", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  const row = sessionRow(page, FIXTURE_LOCAL_HOST, FIXTURE_LEGACY_BASELESS_SESSION_ID);
  await row.click();
  await expect(page.getByTestId("terminal-status")).toContainText("Attached");
  await expect(page.getByRole("heading", { name: "Legacy baseless profile session" })).toBeVisible();
  await expect(page.locator(".session-identity")).toContainText("legacy-profile");
});

test("keeps local work usable when a peer disconnects", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  const peerConnection = hostMarker(page, FIXTURE_PEER_HOST).locator("[data-connection]");
  await expect(peerConnection).toHaveAttribute("data-connection", "connected");

  await stack.peer.scenario.stopAbruptly();

  await expect(peerConnection).toHaveAttribute("data-connection", "error");
  const localSession = sessionRow(page, FIXTURE_LOCAL_HOST, FIXTURE_LOCAL_SESSION_ID);
  await expect(localSession).toBeVisible();
  await localSession.click();
  await expect(page.getByTestId("terminal-status")).toContainText("Attached");
  await expect(page.getByRole("button", { name: "New session", exact: true }).first()).toBeEnabled();
});

test("supports shell shortcuts without stealing terminal input", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  await expect(hostMarker(page, FIXTURE_LOCAL_HOST).locator("[data-connection]"))
    .toHaveAttribute("data-connection", "connected");

  const createDialog = page.getByRole("dialog", { name: "New session" });
  const newSessionButton = page.getByRole("button", { name: "New session", exact: true }).first();
  await newSessionButton.focus();
  await page.keyboard.press("Enter");
  await expect(createDialog).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(createDialog).toBeHidden();

  await page.keyboard.press("Control+b");
  await expect(page.getByRole("button", { name: "Show session rail" })).toBeVisible();
  await page.keyboard.press("Control+b");
  await expect(page.getByRole("button", { name: "Hide session rail" })).toBeVisible();

  await page.keyboard.press("Control+k");
  const palette = page.getByRole("dialog", { name: "Command palette" });
  await expect(palette).toBeVisible();
  await page.keyboard.press("Control+k");
  await expect(palette).toBeHidden();
  await page.getByRole("heading", { name: "Sessions", exact: true }).click();

  await page.keyboard.press("n");
  await expect(createDialog).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(createDialog).toBeHidden();

  await page.keyboard.press("i");
  const inbox = page.getByRole("dialog", { name: "Inbox" });
  await expect(inbox).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(inbox).toBeHidden();

  await page.keyboard.press("/");
  await expect(page.getByRole("searchbox", { name: "Search sessions" })).toBeFocused();
  await page.getByRole("heading", { name: "Sessions", exact: true }).click();

  const initialUrl = page.url();
  await page.keyboard.press("j");
  await expect(page.getByTestId("session-row").first()).toBeFocused();
  await expect(page).toHaveURL(initialUrl);
  await expect(page.getByTestId("terminal-status")).toHaveCount(0);
  await page.keyboard.press("k");
  const focusedSession = page.getByTestId("session-row").last();
  await expect(focusedSession).toBeFocused();
  await expect(page).toHaveURL(initialUrl);
  await expect(page.getByTestId("terminal-status")).toHaveCount(0);
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("terminal-status")).toContainText("Attached");
  await page.getByTestId("terminal").click();
  await page.keyboard.type("n");
  await expect(createDialog).toBeHidden();
});

test("drops a stale persisted selection after host snapshots settle", async ({ page, stack }) => {
  const storageKey = "pohunek.control-center.ui.v1";
  await page.addInitScript(({ key, value }): void => {
    window.localStorage.setItem(key, JSON.stringify(value));
  }, {
    key: storageKey,
    value: {
      sidebarCollapsed: false,
      selectedSession: { host: FIXTURE_LOCAL_HOST, sessionId: "s-stale-selection" },
    },
  });

  await page.goto(stack.backend.url);
  await expect(hostMarker(page, FIXTURE_LOCAL_HOST).locator("[data-connection]"))
    .toHaveAttribute("data-connection", "connected");
  await expect.poll(async () => page.evaluate((key): unknown => {
    const stored = window.localStorage.getItem(key);
    return stored === null ? null : JSON.parse(stored) as unknown;
  }, storageKey)).toEqual({ sidebarCollapsed: false });
  await expect(page).toHaveURL(`${stack.backend.url}/`);
  await expect(page.getByTestId("terminal-status")).toHaveCount(0);
});

function hostMarker(page: Page, host: string): Locator {
  return page.locator(`[data-testid="host-card"][data-host="${host}"]`);
}

function sessionRow(page: Page, host: string, sessionId: string): Locator {
  return page.locator(
    `[data-testid="session-row"][data-host="${host}"][data-session-id="${sessionId}"]`,
  );
}

async function reportNativeIdentity(
  stack: FixtureStackHandle,
  sessionId: string,
  agent: string,
  nativeSessionId: string,
): Promise<void> {
  const address = stack.peer.tcpAddress;
  if (address === undefined) throw new Error("fixture peer did not expose a TCP address");
  const client = await connectTcp(FIXTURE_PEER_HOST, address);
  try {
    const session = await client.call("session.inspect", sessionId);
    const result = await client.call("session.report_native_id", {
      session_id: sessionId,
      runtime_id: `runtime-${sessionId}`,
      agent,
      pid: session.pid,
      pid_start_identity: "playwright-start-identity",
      sequence: "1",
      expires_at: "2099-08-04T00:00:00Z",
      native_session_id: nativeSessionId,
    });
    if (!result.recorded) throw new Error("fixture rejected the native identity report");
  } finally {
    await client.close();
  }
}
