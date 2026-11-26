---
kind: changed
---
The full-screen repaint that `pohunek attach` and the other attach clients receive on attach, and after a queue overflow, is now wrapped in one synchronized-output bracket (`CSI ? 2026 h` ... `CSI ? 2026 l`), so a terminal that supports DEC mode 2026 draws it as one frame instead of a partial or flickering one. A program that has itself set mode 2026 keeps it set after the repaint. Terminals that do not know the mode ignore the sequences. ([#2534](https://github.com/zajca/pohunek/issues/2534))
