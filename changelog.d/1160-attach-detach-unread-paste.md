---
kind: fixed
---
`pohunek attach` now detaches at once when the program does not read a paste that is still queued: the bytes still queued in the client are dropped and the client reports how many. Piped input is finished by closing only the write half, and the client keeps showing output until the daemon closes the stream after delivering the input. A daemon attach also ends when a Unix socket client disconnects while its input waits for the program. A previous-release `pohunek attach` that pipes input into a slow program, and TS SDK clients (which cannot half-close), end their input with a complete close, which a Unix-socket daemon now treats as a disconnect: input the terminal had not accepted is dropped.
