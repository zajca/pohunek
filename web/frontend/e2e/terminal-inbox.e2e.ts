import type { Locator, Page } from "@playwright/test";
import {
  FIXTURE_LOCAL_HOST,
  FIXTURE_LOCAL_SESSION_ID,
  FIXTURE_NOTIFICATION_ID,
  FIXTURE_PEER_HOST,
  FIXTURE_PEER_SESSION_ID,
} from "../../scripts/fixture-stack";
import { expect, test } from "./fixtures";

test("keeps the session rail visible while attaching and switching terminals", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  await expect(hostMarker(page, FIXTURE_LOCAL_HOST).locator("[data-connection]"))
    .toHaveAttribute("data-connection", "connected");

  await sessionRow(page, FIXTURE_LOCAL_HOST, FIXTURE_LOCAL_SESSION_ID).click();
  await expect(page.getByTestId("terminal-status")).toContainText("Attached");
  await expect(page.getByRole("complementary", { name: "Sessions" })).toBeVisible();
  await expect.poll(
    () => stack.local.scenario.initialAttachDimensions(FIXTURE_LOCAL_SESSION_ID).length,
  )
    .toBeGreaterThan(0);

  const terminal = page.getByTestId("terminal");
  await terminal.click();
  await page.keyboard.type("first-attach-echo");
  await expect(terminal.locator(".xterm-accessibility-tree")).toContainText("first-attach-echo");

  await sessionRow(page, FIXTURE_PEER_HOST, FIXTURE_PEER_SESSION_ID).click();
  await expect(page.getByTestId("terminal-status")).toContainText("Attached");
  await sessionRow(page, FIXTURE_LOCAL_HOST, FIXTURE_LOCAL_SESSION_ID).click();
  await expect(page.getByTestId("terminal-status")).toContainText("Attached");
  await page.getByTestId("terminal").click();
  await page.keyboard.type("second-attach-echo");
  await expect(page.locator(".xterm-accessibility-tree")).toContainText("second-attach-echo");
});

test("opens a notification's session from the inbox and acknowledges live records", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  await expect(page.getByTestId("unread-count")).toHaveText("1");
  await page.getByRole("button", { name: /Inbox/ }).click();

  const inbox = page.getByRole("dialog", { name: "Inbox" });
  await expect(inbox.locator(":focus")).toHaveAttribute("aria-label", "Close inbox");
  await expect(page.locator('button[aria-label="Close inbox"][tabindex="-1"]')).toHaveCount(1);
  await page.keyboard.press("Shift+Tab");
  await expect(inbox.locator(":focus")).toHaveCount(1);
  const seeded = notificationCard(inbox, FIXTURE_NOTIFICATION_ID);
  await expect(seeded).toContainText("Approval required");
  await seeded.getByRole("button", { name: "Open session" }).click();
  await expect(inbox).toBeHidden();
  await expect(sessionRow(page, FIXTURE_LOCAL_HOST, FIXTURE_LOCAL_SESSION_ID))
    .toHaveAttribute("aria-current", "page");
  await expect(page.getByTestId("terminal-status")).toContainText("Attached");
  await expect(page.getByTestId("unread-count")).toHaveCount(0);

  const live = stack.local.scenario.createNotification({
    id: "n-browser-live",
    source: {
      provider: "pohunek-testkit",
      provider_event: "browser-e2e",
      host_local_source_id: "browser-live-notification",
    },
    kind: "system",
    severity: "info",
    title: "Live browser notification",
    body: "Delivered through the active subscription.",
  });
  await expect(page.getByTestId("unread-count")).toHaveText("1");
  await page.getByRole("button", { name: /Inbox/ }).click();

  const liveCard = notificationCard(inbox, live.id);
  await expect(liveCard).toContainText("Live browser notification");
  await liveCard.getByRole("button", { name: "Acknowledge" }).click();
  await expect(liveCard).toBeHidden();
  await expect(page.getByTestId("unread-count")).toHaveCount(0);
});

function hostMarker(page: Page, host: string): Locator {
  return page.locator(`[data-testid="host-card"][data-host="${host}"]`);
}

function sessionRow(page: Page, host: string, sessionId: string): Locator {
  return page.locator(
    `[data-testid="session-row"][data-host="${host}"][data-session-id="${sessionId}"]`,
  );
}

function notificationCard(scope: Locator, notificationId: string): Locator {
  return scope.locator(
    `[data-testid="notification-card"][data-notification-id="${notificationId}"]`,
  );
}
