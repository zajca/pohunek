## Unreleased (2026-11-11, composer_absent detection matcher, #1968)

- A detection manifest rule may add `composer_absent = true` (runtime feature `detect-composer-absent-v1`): the rule applies only while the screen shows no prompt box, so a dialog that replaces the composer fails closed. See [Runtime packages](guides/runtime-packages.md#dialog-rules-that-fail-closed).
