---
kind: fixed
---
A slow attach client no longer delays its own typing. When the terminal, a remote link or a busy host reads the agent's output slowly, keystrokes still reach the agent without waiting for the output to be written, and output the client cannot keep up with is replaced by a fresh repaint of the screen instead of piling up in memory.
