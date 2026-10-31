---
kind: fixed
---
The Rust client and the TypeScript SDK treat bytes at the end of a control stream that have no terminating newline as a closed connection and drop them. Before, such a fragment (an event the daemon was writing when it stopped) was returned as a line and surfaced as a parse or UTF-8 error; now a subscription ends with no event and no error.
