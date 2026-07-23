import { fileURLToPath } from "node:url";
import { expect, test as base } from "@playwright/test";
import {
  startFixtureStack,
  type FixtureStackHandle,
} from "../../scripts/fixture-stack";

const FRONTEND_DIST_DIR = fileURLToPath(new URL("../dist/", import.meta.url));

interface FrontendFixtures {
  readonly stack: FixtureStackHandle;
}

export const test = base.extend<FrontendFixtures>({
  stack: async ({}, use): Promise<void> => {
    const stack = await startFixtureStack({ staticAssetsDir: FRONTEND_DIST_DIR });
    try {
      await use(stack);
    } finally {
      await stack.close();
    }
  },
});

export { expect };
