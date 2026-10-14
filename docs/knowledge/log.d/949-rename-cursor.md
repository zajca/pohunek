## Unreleased (2026-10-14, attach menu rename cursor, #949)

- The cursor of the attach menu's rename input sits right after the last typed
  character (after `Name: ` when the input is empty). It used to sit one cell
  to the left, on the last character. The overlay cursor column is now counted
  from the first text cell of its line, so a caller no longer has to know the
  box's padding.
