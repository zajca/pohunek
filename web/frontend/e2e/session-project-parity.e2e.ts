import { expect, test } from "./fixtures";
import {
  FIXTURE_EXTERNAL_SESSION_ID,
  FIXTURE_LOCAL_HOST,
  FIXTURE_LOCAL_SESSION_ID,
  FIXTURE_OWNED_WORKTREE_PATH,
} from "../../scripts/fixture-stack";

test("manages a session lifecycle and keeps observed sessions read-only", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  await page.locator(`[data-testid="session-row"][data-host="${FIXTURE_LOCAL_HOST}"][data-session-id="${FIXTURE_LOCAL_SESSION_ID}"]`).click();

  await page.getByRole("button", { name: "Rename", exact: true }).click();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog", { name: "Rename session" })).toBeHidden();
  await expect(page.getByRole("button", { name: "Rename", exact: true })).toBeFocused();
  await page.getByRole("button", { name: "Rename", exact: true }).click();
  const rename = page.getByRole("dialog", { name: "Rename session" });
  await rename.getByRole("textbox", { name: "Name" }).fill("Renamed from browser");
  await rename.getByRole("button", { name: "Save" }).click();
  await expect(page.getByRole("heading", { name: "Renamed from browser" })).toBeVisible();

  await page.getByRole("button", { name: "Details", exact: true }).click();
  const inspector = page.getByRole("dialog", { name: "Renamed from browser" });
  await inspector.getByRole("button", { name: "Set metadata" }).click();
  const metadata = page.getByRole("dialog", { name: "Set metadata" });
  await metadata.getByRole("textbox", { name: "Key" }).fill("work_item");
  await metadata.getByRole("textbox", { name: "Value" }).fill("ABC-123");
  await metadata.getByRole("button", { name: "Save" }).click();
  await expect(inspector).toContainText("work_item");
  await inspector.getByRole("button", { name: "Close session details" }).click();

  await page.getByRole("button", { name: "Fork", exact: true }).click();
  const fork = page.getByRole("dialog", { name: "Fork session" });
  await fork.getByRole("textbox", { name: "Display name (optional)" }).fill("Forked browser session");
  await fork.getByRole("spinbutton", { name: "Columns" }).fill("132");
  await fork.getByRole("spinbutton", { name: "Rows" }).fill("43");
  await fork.getByRole("button", { name: "Save" }).click();
  await expect(page.getByRole("heading", { name: "Forked browser session" })).toBeVisible();
  await page.getByRole("button", { name: "Stop", exact: true }).click();
  await page.getByRole("dialog", { name: "Stop this session?" }).getByRole("button", { name: "Stop session" }).click();
  await expect(page.getByRole("button", { name: "Resume", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Resume", exact: true }).click();
  await expect(page.getByRole("button", { name: "Stop", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Remove", exact: true }).click();
  await page.getByRole("dialog", { name: "Remove this session?" }).getByRole("button", { name: "Remove session" }).click();
  await expect(page).toHaveURL(`${stack.backend.url}/`);

  await page.locator(`[data-testid="session-row"][data-host="${FIXTURE_LOCAL_HOST}"][data-session-id="${FIXTURE_EXTERNAL_SESSION_ID}"]`).click();
  await expect(page.getByTestId("session-summary")).toContainText("Observe-only session");
  for (const name of ["Open terminal", "Stop", "Resume", "Rename", "Fork", "Remove"]) {
    await expect(page.getByRole("button", { name, exact: true })).toHaveCount(0);
  }
});

test("manages host-scoped projects and eligible owned worktrees", async ({ page, stack }) => {
  await page.goto(stack.backend.url);
  await page.getByRole("button", { name: "Projects" }).click();
  await expect(page).toHaveURL(new RegExp(`/hosts/${FIXTURE_LOCAL_HOST}/projects$`));
  await page.getByRole("combobox", { name: "Project host" }).selectOption("fixture-peer");
  await expect(page).toHaveURL(/\/hosts\/fixture-peer\/projects$/u);
  await expect(page.getByRole("button", { name: "Add project" })).toBeEnabled();
  await page.getByRole("combobox", { name: "Project host" }).selectOption(FIXTURE_LOCAL_HOST);
  await expect(page).toHaveURL(new RegExp(`/hosts/${FIXTURE_LOCAL_HOST}/projects$`));
  await page.getByRole("button", { name: "Fixture project" }).click();
  await expect(page.getByTestId("projects-screen")).toContainText(FIXTURE_OWNED_WORKTREE_PATH);
  await page.getByRole("button", { name: "Remove worktree" }).click();
  await page.getByRole("dialog", { name: "Remove this worktree?" }).getByRole("button", { name: "Remove worktree" }).click();
  await expect(page.getByTestId("projects-screen")).not.toContainText(FIXTURE_OWNED_WORKTREE_PATH);

  await page.getByRole("button", { name: "Rename" }).click();
  const rename = page.getByRole("dialog", { name: "Rename project" });
  await rename.getByRole("textbox", { name: "Display name" }).fill("Renamed fixture project");
  await rename.getByRole("button", { name: "Save" }).click();
  await expect(page.getByRole("heading", { name: "Renamed fixture project" })).toBeVisible();
  await page.getByRole("button", { name: "All projects" }).click();
  await page.getByRole("button", { name: "Add project" }).click();
  const add = page.getByRole("dialog", { name: "Add project" });
  await add.getByRole("textbox", { name: "Absolute path" }).fill("/tmp/browser-project");
  await add.getByRole("textbox", { name: "Display name (optional)" }).fill("Browser project");
  await add.getByRole("button", { name: "Save" }).click();
  await expect(page.getByRole("heading", { name: "Browser project" })).toBeVisible();
  await page.getByRole("button", { name: "Remove project" }).click();
  const remove = page.getByRole("dialog", { name: "Remove this project?" });
  await remove.getByRole("checkbox").check();
  await remove.getByRole("button", { name: "Remove project" }).click();
  await expect(page).toHaveURL(new RegExp(`/hosts/${FIXTURE_LOCAL_HOST}/projects$`));
  await expect(page.getByRole("button", { name: "Browser project" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Renamed fixture project" })).toBeVisible();
  await page.getByRole("button", { name: "Back to workspace" }).click();
  await expect(page).toHaveURL(`${stack.backend.url}/`);
});
