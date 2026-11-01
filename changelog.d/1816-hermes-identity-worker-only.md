---
kind: fixed
---
The Hermes runtime package is version 1.0.4. Its hook reporter sends a session's native id to the session worker only: the daemon always refused the public native-id report for Hermes sessions, so the fallback after a slow or refused worker reply never worked and logged a false `session.native_report.unattested` warning each time, and it blocked the Hermes turn a second time. The warning is gone. A report whose request was fully written to the worker is applied whether or not its reply arrives in time, and a missed reply is no longer counted as a hook failure; a refusal, a connection the worker closes without a reply, and a failure to connect or write still are. A package update installs the new reporter.
