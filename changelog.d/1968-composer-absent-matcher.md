---
kind: added
---
A detection manifest rule can require that no composer is on screen with `composer_absent = true`, so a dialog rule that replaces the composer fails closed even when quoted model text contains a horizontal-rule line. A package that uses it lists the new runtime feature `detect-composer-absent-v1` in `requires`; without the entry the package is refused with `runtime_feature_unsupported`, and an older core refuses it for the same reason.
