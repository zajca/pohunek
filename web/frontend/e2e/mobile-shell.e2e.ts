import type { Locator, Page } from "@playwright/test";
import {
  FIXTURE_LOCAL_HOST,
  FIXTURE_LOCAL_SESSION_ID,
} from "../../scripts/fixture-stack";
import { expect, test } from "./fixtures";

test.describe("portrait mobile shell", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("uses an accessible off-canvas session drawer", async ({ page, stack }) => {
    await page.goto(stack.backend.url);

    const menu = page.getByRole("button", { name: "Open session navigation" });
    const navigation = page.getByRole("dialog", { name: "Session navigation" });
    await expect(menu).toBeVisible();
    await expect(navigation).toBeHidden();
    await expectTouchTarget(menu);

    await menu.click();
    await expect(navigation).toBeVisible();
    await expect(navigation.getByRole("button", { name: "Close session navigation" })).toBeFocused();
    await expect(page.locator(".workspace-topbar")).toHaveAttribute("inert", "");
    await expect(page.locator(".session-main-container")).toHaveAttribute("inert", "");

    const close = navigation.getByRole("button", { name: "Close session navigation" });
    await expectTouchTarget(close);

    const firstTabStop = navigation.getByRole("button", { name: "New session" });
    const lastTabStop = navigation.locator('[data-testid="session-row"][tabindex="0"]');
    await firstTabStop.focus();
    await page.keyboard.press("Shift+Tab");
    await expect(lastTabStop).toBeFocused();
    await lastTabStop.focus();
    await page.keyboard.press("Tab");
    await expect(firstTabStop).toBeFocused();

    await close.click();
    await expect(navigation).toBeHidden();
    await expect(menu).toBeFocused();

    await menu.click();
    await page.keyboard.press("Escape");
    await expect(navigation).toBeHidden();
    await expect(menu).toBeFocused();

    await menu.click();
    await page.locator(".mobile-rail-backdrop").click({ position: { x: 385, y: 422 } });
    await expect(navigation).toBeHidden();

    await menu.click();
    await navigation.locator(
      `[data-testid="session-row"][data-host="${FIXTURE_LOCAL_HOST}"][data-session-id="${FIXTURE_LOCAL_SESSION_ID}"]`,
    ).click();
    await expect(navigation).toBeHidden();
    await expect(page.getByTestId("terminal-status")).toContainText("Attached");
    await expect(menu).toBeFocused();
    await expectNoHorizontalOverflow(page);
  });

  test("provides touch terminal keys and an explicit software-keyboard focus", async ({ page, stack }) => {
    await page.goto(stack.backend.url);
    await page.getByRole("button", { name: "Open session navigation" }).click();
    await page.locator(
      `[data-testid="session-row"][data-host="${FIXTURE_LOCAL_HOST}"][data-session-id="${FIXTURE_LOCAL_SESSION_ID}"]`,
    ).click();
    await expect(page.getByTestId("terminal-status")).toContainText("Attached");

    const toolbar = page.getByRole("toolbar", { name: "Mobile terminal controls" });
    await expect(toolbar).toBeVisible();
    for (const control of await toolbar.getByRole("button").all()) {
      await expectTouchTarget(control);
    }

    await toolbar.getByRole("button", { name: "Focus terminal and open keyboard" }).click();
    await expect(page.locator(".xterm-helper-textarea")).toBeFocused();
    await page.keyboard.type("mobile-toolbar-echo");
    await expect(page.locator(".xterm-accessibility-tree")).toContainText("mobile-toolbar-echo");

    const control = toolbar.getByRole("button", { name: "Toggle Control modifier" });
    await control.click();
    await expect(control).toHaveAttribute("aria-pressed", "true");
    await toolbar.getByRole("button", { name: "Send up arrow" }).click();
    await expect(control).toHaveAttribute("aria-pressed", "false");
  });
});

test.describe("landscape touch shell", () => {
  test.use({ viewport: { width: 844, height: 390 }, hasTouch: true, isMobile: true });

  test("keeps the rail off-canvas so the terminal owns the viewport", async ({ page, stack }) => {
    await page.goto(stack.backend.url);

    const menu = page.getByRole("button", { name: "Open session navigation" });
    const navigation = page.getByRole("dialog", { name: "Session navigation" });
    await expect(menu).toBeVisible();
    await expect(navigation).toBeHidden();
    await menu.click();
    await expect(navigation).toBeVisible();
    await navigation.getByRole("button", { name: "Close session navigation" }).click();
    await expect(navigation).toBeHidden();
    await expectNoHorizontalOverflow(page);
  });
});

test("hides touch terminal controls on desktop", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  await page.locator(
    `[data-testid="session-row"][data-host="${FIXTURE_LOCAL_HOST}"][data-session-id="${FIXTURE_LOCAL_SESSION_ID}"]`,
  ).click();
  await expect(page.getByTestId("terminal-status")).toContainText("Attached");
  await expect(page.getByRole("toolbar", { name: "Mobile terminal controls" })).toBeHidden();
});

async function expectTouchTarget(locator: Locator): Promise<void> {
  const box = await locator.boundingBox();
  expect(box).not.toBeNull();
  expect(box?.width ?? 0).toBeGreaterThanOrEqual(44);
  expect(box?.height ?? 0).toBeGreaterThanOrEqual(44);
}

async function expectNoHorizontalOverflow(page: Page): Promise<void> {
  const overflow = await page.evaluate((): number => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
}
