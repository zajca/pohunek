---
kind: fixed
---
A needs-you notification no longer outlives its session: removing a session acknowledges its open attention records, and the daemon acknowledges at startup every open record whose session is gone, ended or on a lost runtime, so a daemon upgrade or a host restart no longer leaves a record that makes a desktop notifier name a session that does not wait. ([#2489](https://github.com/zajca/pohunek/issues/2489))
