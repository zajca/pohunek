---
kind: fixed
---
Typing in `pohunek attach` no longer stalls while the terminal is slow to take the agent's output. Keystrokes, shortcuts and the menu keep working while a terminal write is pending, and a slow terminal pushes back on the stream instead of growing a buffer.
