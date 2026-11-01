---
kind: added
---
On macOS a lost session now records why its worker ended. The daemon watches each worker process through the kernel and reports a non-zero exit code, a terminating signal, or a kill under memory pressure in `runtime.exit` (`pohunek session inspect`), as it already did on Linux. A worker that ended while the daemon was not running still has no recorded cause.
