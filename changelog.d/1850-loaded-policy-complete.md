---
kind: fixed
---
A notification policy that a hand-edited `policy.json` leaves a kind out of is completed with the daemon's default for that kind when the daemon loads it, so `notifications policy get` and `notification.policy.get` return a flag for every kind instead of a partial map. The file itself is not rewritten until the policy is next set.
