## Unreleased (2026-10-31, attach input ack contract and blocked-write disconnect, #1568 #1160)

- The attach `InputAck` is documented as unobserved: no client relies on it and the raw public stream carries none, so the stream may end at program exit without the acknowledgement of a frame the terminal accepted (#1568).
- A daemon attach bridge whose send to the program waits ends when the client closes a Unix socket connection completely; TCP and relay connections keep the attach until the program reads, the session stops or the lease is released (#1160).
- The `pohunek attach` client reads up to 1 MiB of typed input ahead while the program does not read, so the detach shortcut is seen behind a paste. The detach delivers what the stream took, drops the rest and prints the dropped byte count (#1160).
