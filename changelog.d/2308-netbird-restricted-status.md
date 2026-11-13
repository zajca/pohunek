---
kind: fixed
---
A connected NetBird host with no peers no longer fails to parse (`details: null`, any NetBird version). Under NetBird 0.83+, an account that does not own the NetBird connection gets `netbird_state_restricted`, naming the cause and the fix, instead of a parse error. ([#2308](https://github.com/zajca/pohunek/issues/2308))
