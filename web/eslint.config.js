import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import svelte from "eslint-plugin-svelte";
import tseslint from "typescript-eslint";

const tsconfigRootDir = dirname(fileURLToPath(import.meta.url));

export default tseslint.config(
  {
    ignores: [
      "**/generated/**",
      "**/dist/**",
      "**/node_modules/**"
    ]
  },
  ...svelte.configs["flat/recommended"],
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
  },
  {
    files: [
      "**/*.svelte"
    ],
    plugins: {
      "@typescript-eslint": tseslint.plugin
    },
    languageOptions: {
      parserOptions: {
        extraFileExtensions: [".svelte"],
        parser: tseslint.parser,
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
