import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import tseslint from "typescript-eslint";

const tsconfigRootDir = dirname(fileURLToPath(import.meta.url));

export default tseslint.config(
  {
    ignores: [
      "**/generated/**",
      "**/dist/**",
      "**/dist-types/**",
      "**/node_modules/**"
    ]
  },
  {
    files: [
      "**/*.ts"
    ],
    plugins: {
      "@typescript-eslint": tseslint.plugin
    },
    extends: [
      ...tseslint.configs.recommendedTypeChecked
    ],
    languageOptions: {
      parserOptions: {
        projectService: true,
        tsconfigRootDir
      }
    },
    rules: {
      "@typescript-eslint/explicit-function-return-type": [
        "error",
        {
          "allowExpressions": true,
          "allowHigherOrderFunctions": true,
          "allowTypedFunctionExpressions": true
        }
      ]
    }
  }
);
