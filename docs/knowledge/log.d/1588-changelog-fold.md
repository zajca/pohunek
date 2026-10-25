## Unreleased (2026-10-25, changelog, #1588)

- `scripts/release` folds the `changelog.d/` fragments into the committed `CHANGELOG.md` in the release commit, next to the knowledge-log fold. It refuses a candidate that still holds fragments or has no folded section for its version, and `--dry-run` prints the section the release body will carry.
