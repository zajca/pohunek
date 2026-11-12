---
kind: fixed
---
`pohunek attach` now recognises the arrow-key reports every terminal encodes- event-typed kitty (`ESC [ 1 ; 1 : 1 A`), SS3 (`ESC O A`, application cursor mode) and legacy CSI alike, instead of reading their parameter bytes as literal keys, so menu selection moves under foot, Claude Code and any terminal that reports key-event types. Escape closes the menu in its CSI-u form as well, and the release that follows it no longer leaks to the attached program after the menu closes. Other CSI reports (focus, colour, mode) are consumed whole by the open menu instead of acting as hotkeys or landing in the rename buffer.
