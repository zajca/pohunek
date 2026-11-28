---
kind: fixed
---
The screen tracker now reads a `DEL` or a C0 control inside a CSI sequence the way a VT parser does — ignored or executed without ending the sequence — so `ESC [ DEL ? 2026 h` sets mode 2026 like a real terminal and the attach repaint no longer appends its closing reset for a program that holds synchronized output in that case. ([#2577](https://github.com/zajca/pohunek/issues/2577))
