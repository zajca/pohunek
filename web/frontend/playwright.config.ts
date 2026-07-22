import { defineConfig } from "@playwright/test";

const TEST_TIMEOUT_MS = 20_000;
const EXPECT_TIMEOUT_MS = 8_000;

export default defineConfig({
  testDir: "./e2e",
  testMatch: "**/*.e2e.ts",
  fullyParallel: false,
  workers: 1,
  timeout: TEST_TIMEOUT_MS,
  expect: {
    timeout: EXPECT_TIMEOUT_MS,
  },
  use: {
    browserName: "chromium",
    headless: true,
    trace: "retain-on-failure",
  },
  outputDir: "test-results",
});
