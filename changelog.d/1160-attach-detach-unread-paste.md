---
kind: fixed
---
`pohunek attach` now detaches at once when the program does not read a paste that is still queued: bytes the stream already took are delivered in order, the rest are dropped and the client reports how many. A daemon attach also ends when a Unix socket client disconnects while its input waits for the program.
