---
kind: fixed
---
`pohunek attach` now recognises every arrow-key report a terminal encodes: event-typed kitty (`ESC [ 1 ; 1 : 1 A`, used by Codex at `>7u` in foot), SS3 (`ESC O A`, application cursor mode) and legacy CSI alike, instead of reading their parameter bytes as literal keys, so the menu selection moves under foot and every other terminal that reports key-event types. The Escape release that follows the key that closed the menu no longer leaks to the attached program after the menu closes, including for a key that was still held. Other CSI reports (focus, colour, mode) are consumed whole by the open menu instead of acting as hotkeys or landing in the rename buffer. [#2281](https://github.com/zajca/pohunek/issues/2281)
