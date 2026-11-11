## Unreleased (2026-11-11, composer_absent limits, #2123)

- The `composer_absent` guidance now states that the prompt-box fact counts every horizontal-rule line on the visible grid, so the matcher only suits a dialog that draws no rule line of its own; a dialog that draws one rule is defeated by a single quoted rule line. See [Runtime packages](guides/runtime-packages.md#dialog-rules-that-fail-closed).
