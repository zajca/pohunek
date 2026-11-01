---
kind: changed
---
A client now keeps a key it does not know in the session capabilities of a listed session and in the session and notification retention and policy documents a newer daemon returns, so `session list` no longer fails on a newer capability and `session policy set` and `notifications policy set` send every key back instead of resetting a setting they do not know. The daemon still refuses such a key in params, in the session store and in `policy.json` and `session-policy.json`, and a document with no unknown key serializes exactly as before.
